//! Measure a recorded file with host ffmpeg filters (no libav).
//!
//! Two signals, both read from ffmpeg's `metadata` / `ametadata` printer:
//!
//! * **frame difference** — `signalstats` `YDIF` (mean absolute luma delta
//!   against the previous frame), on a downscaled, decimated copy;
//! * **audio energy** — `astats` `Overall.RMS_level` in dBFS over fixed
//!   windows.
//!
//! Measurement is best-effort: a missing binary, a missing stream, or a
//! filter this ffmpeg build does not have yields `Ok(None)`. Callers must
//! treat that as *unknown*, never as *quiet*.

use reelforge_capture_core::{CaptureError, HZ_1K, MediaTime, Result, SignalKind, SignalTrack};
use std::path::Path;
use std::process::Command;

/// Frame-difference metadata key exported by `signalstats`.
pub const MOTION_KEY: &str = "lavfi.signalstats.YDIF";
/// Audio energy metadata key exported by `astats`.
pub const AUDIO_RMS_KEY: &str = "lavfi.astats.Overall.RMS_level";
/// dBFS value used for digital silence (`astats` prints `-inf`).
pub const SILENCE_FLOOR_DB: f64 = -120.0;

/// Frame-difference series for the video stream of `path`.
///
/// `sample_fps` is the decimation rate (2.0 = one measurement every 500 ms).
/// Samples are stamped from the start of the file; the caller offsets them
/// onto the session clock.
///
/// # Errors
///
/// Invalid `sample_fps`, or ffmpeg I/O other than a missing binary.
pub fn motion_series(path: impl AsRef<Path>, sample_fps: f64) -> Result<Option<SignalTrack>> {
    if !(sample_fps.is_finite() && sample_fps > 0.0) {
        return Err(CaptureError::message(format!(
            "sample fps must be > 0 (got {sample_fps})"
        )));
    }
    let filter =
        format!("fps={sample_fps},scale=160:-2,signalstats,metadata=print:key={MOTION_KEY}:file=-");
    let Some(raw) = run_ffmpeg(&["-an".into(), "-vf".into(), filter], path.as_ref())? else {
        return Ok(None);
    };
    let period = MediaTime::from_secs(1.0 / sample_fps, HZ_1K)?;
    Ok(fill(
        SignalTrack::new(SignalKind::Motion, "video", period),
        &raw,
        MOTION_KEY,
        None,
    ))
}

/// Audio energy (dBFS) for one audio stream of `path`.
///
/// `audio_index` is the `a:N` index inside the container — from
/// [`AudioMix::audio_index`](reelforge_capture_core::AudioMix::audio_index)
/// for a muxed segment, or `0` for a demuxed per-leg file.
///
/// # Errors
///
/// Invalid `window_secs`, or ffmpeg I/O other than a missing binary.
pub fn audio_level_series(
    path: impl AsRef<Path>,
    audio_index: u32,
    window_secs: f64,
    source: &str,
) -> Result<Option<SignalTrack>> {
    if !(window_secs.is_finite() && window_secs > 0.0) {
        return Err(CaptureError::message(format!(
            "audio window must be > 0 (got {window_secs})"
        )));
    }
    // Resample to a fixed rate so the window length is exact in samples.
    let rate = 8_000.0_f64;
    let n = (rate * window_secs).round().max(1.0);
    let filter = format!(
        "aresample={rate:.0},asetnsamples=n={n:.0}:p=0,astats=metadata=1:reset=1,ametadata=print:key={AUDIO_RMS_KEY}:file=-"
    );
    let args = vec![
        "-vn".into(),
        "-map".into(),
        format!("0:a:{audio_index}"),
        "-af".into(),
        filter,
    ];
    let Some(raw) = run_ffmpeg(&args, path.as_ref())? else {
        return Ok(None);
    };
    let period = MediaTime::from_secs(window_secs, HZ_1K)?;
    Ok(fill(
        SignalTrack::new(SignalKind::AudioLevel, source, period),
        &raw,
        AUDIO_RMS_KEY,
        Some(SILENCE_FLOOR_DB),
    ))
}

/// Demux one audio stream into its own file (`-c copy`, no re-encode).
///
/// This is what makes an audio track in a `CaptureProject` unambiguous: the
/// media entry points at a file with exactly one stream instead of at a
/// container where the reader would have to guess.
///
/// # Errors
///
/// Missing ffmpeg, missing stream, or a non-zero exit (stderr is included).
pub fn extract_audio_stream(
    src: impl AsRef<Path>,
    audio_index: u32,
    dst: impl AsRef<Path>,
) -> Result<()> {
    let program = ffmpeg_program();
    let out = Command::new(&program)
        .args(["-hide_banner", "-v", "error", "-y", "-i"])
        .arg(src.as_ref())
        .args(["-map", &format!("0:a:{audio_index}"), "-vn", "-sn", "-dn"])
        .args(["-c", "copy"])
        .arg(dst.as_ref())
        .output()
        .map_err(|e| CaptureError::io(format!("{program} extract: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    let why = String::from_utf8_lossy(&out.stderr);
    Err(CaptureError::io(format!(
        "extract a:{audio_index} from {}: {}",
        src.as_ref().display(),
        why.trim()
    )))
}

/// Parse ffmpeg `metadata=print` output into `(seconds, value)` pairs.
///
/// The printer emits a `frame:… pts_time:X` header followed by `key=value`
/// lines. Values ffmpeg writes as `-inf` / `inf` come back as infinities;
/// `nan` samples are dropped.
#[must_use]
pub fn parse_metadata_series(raw: &str, key: &str) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    let mut t: Option<f64> = None;
    for line in raw.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("frame:") {
            t = rest
                .split_whitespace()
                .find_map(|f| f.strip_prefix("pts_time:"))
                .and_then(|v| v.parse::<f64>().ok());
            continue;
        }
        let Some(value) = line.strip_prefix(key).and_then(|r| r.strip_prefix('=')) else {
            continue;
        };
        let Some(secs) = t else { continue };
        let parsed = match value.trim() {
            "-inf" | "-INF" => f64::NEG_INFINITY,
            "inf" | "INF" | "+inf" => f64::INFINITY,
            other => match other.parse::<f64>() {
                Ok(v) => v,
                Err(_) => continue,
            },
        };
        if parsed.is_nan() {
            continue;
        }
        out.push((secs, parsed));
    }
    out
}

/// Stamp parsed pairs onto a track. `floor` replaces `-inf` (digital
/// silence); without one, non-finite samples are dropped.
fn fill(mut track: SignalTrack, raw: &str, key: &str, floor: Option<f64>) -> Option<SignalTrack> {
    for (secs, value) in parse_metadata_series(raw, key) {
        let value = match (value.is_finite(), floor) {
            (true, _) => value,
            (false, Some(f)) if value.is_sign_negative() => f,
            (false, _) => continue,
        };
        let Ok(t) = MediaTime::from_secs(secs, HZ_1K) else {
            continue;
        };
        track.push(t, value);
    }
    track.is_measured().then_some(track)
}

fn ffmpeg_program() -> String {
    std::env::var("REELFORGE_FFMPEG").unwrap_or_else(|_| "ffmpeg".into())
}

/// Run a measuring ffmpeg pass. `Ok(None)` when ffmpeg is absent or refuses
/// the file / filter (unknown, not quiet).
fn run_ffmpeg(extra: &[String], path: &Path) -> Result<Option<String>> {
    let program = ffmpeg_program();
    let out = match Command::new(&program)
        .args(["-hide_banner", "-v", "error", "-nostats", "-i"])
        .arg(path)
        .args(extra)
        .args(["-f", "null", "-"])
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CaptureError::io(format!("{program} measure: {e}"))),
    };
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOTION_DUMP: &str = "\
frame:0    pts:0       pts_time:0
lavfi.signalstats.YDIF=0
frame:1    pts:1       pts_time:0.5
lavfi.signalstats.YDIF=3.81005
frame:2    pts:2       pts_time:1
lavfi.signalstats.YDIF=nan
";

    const AUDIO_DUMP: &str = "\
frame:0    pts:0       pts_time:0
lavfi.astats.Overall.RMS_level=-inf
frame:1    pts:4000    pts_time:0.5
lavfi.astats.Overall.RMS_level=-21.069741
";

    #[test]
    fn reads_frame_difference_pairs() {
        let s = parse_metadata_series(MOTION_DUMP, MOTION_KEY);
        assert_eq!(s.len(), 2, "nan sample is dropped: {s:?}");
        assert!((s[0].0 - 0.0).abs() < 1e-9);
        assert!((s[1].1 - 3.81005).abs() < 1e-6);
    }

    #[test]
    fn silence_becomes_the_floor_not_a_hole() {
        let track = fill(
            SignalTrack::new(
                SignalKind::AudioLevel,
                "audio:system",
                MediaTime::from_secs(0.5, HZ_1K).unwrap(),
            ),
            AUDIO_DUMP,
            AUDIO_RMS_KEY,
            Some(SILENCE_FLOOR_DB),
        )
        .expect("measured");
        assert_eq!(track.samples.len(), 2);
        assert!((track.samples[0].value - SILENCE_FLOOR_DB).abs() < 1e-9);
        assert!((track.samples[1].value + 21.069_741).abs() < 1e-6);
    }

    #[test]
    fn a_single_sample_is_not_a_measurement() {
        let one = "frame:0    pts:0       pts_time:0\nlavfi.signalstats.YDIF=0\n";
        assert!(
            fill(
                SignalTrack::new(
                    SignalKind::Motion,
                    "video",
                    MediaTime::from_secs(0.5, HZ_1K).unwrap(),
                ),
                one,
                MOTION_KEY,
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn other_keys_are_ignored() {
        let mixed = "\
frame:0    pts:0       pts_time:0
lavfi.signalstats.YMIN=16
lavfi.signalstats.YDIF=1.5
";
        let s = parse_metadata_series(mixed, MOTION_KEY);
        assert_eq!(s, vec![(0.0, 1.5)]);
    }

    #[test]
    fn rejects_impossible_sampling_rates() {
        assert!(motion_series("x.mkv", 0.0).is_err());
        assert!(audio_level_series("x.mkv", 0, -1.0, "audio:system").is_err());
    }
}
