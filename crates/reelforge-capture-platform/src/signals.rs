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
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
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

/// Everything measured in a single decode pass over one segment.
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentSignals {
    /// Frame difference, when video was measured.
    pub motion: Option<SignalTrack>,
    /// Audio energy per requested stream, in the order they were requested.
    pub audio: Vec<SignalTrack>,
}

/// Measure the picture and every listed audio stream in **one** ffmpeg pass.
///
/// Running a pass per signal spawns (and decodes) once per signal; this walks
/// the file once. Each leg is downmixed to mono and merged, so `astats`
/// reports it under its own channel key — the printers share stdout but never
/// share a key, which is what makes the output unambiguous.
///
/// `Ok(None)` means the pass did not produce anything usable (no ffmpeg, a
/// missing stream, a filter this build lacks). Callers should fall back to
/// [`motion_series`] / [`audio_level_series`] rather than treat it as silence.
///
/// # Errors
///
/// Invalid sampling parameters, or ffmpeg I/O other than a missing binary.
pub fn measure_segment(
    path: impl AsRef<Path>,
    sample_fps: Option<f64>,
    audio_indices: &[u32],
    window_secs: f64,
) -> Result<Option<SegmentSignals>> {
    if sample_fps.is_none() && audio_indices.is_empty() {
        return Ok(None);
    }
    if let Some(fps) = sample_fps
        && !(fps.is_finite() && fps > 0.0)
    {
        return Err(CaptureError::message(format!(
            "sample fps must be > 0 (got {fps})"
        )));
    }
    if !audio_indices.is_empty() && (!window_secs.is_finite() || window_secs <= 0.0) {
        return Err(CaptureError::message(format!(
            "audio window must be > 0 (got {window_secs})"
        )));
    }

    let (graph, maps) = measure_graph(sample_fps, audio_indices, window_secs);
    let mut args = vec!["-filter_complex".to_string(), graph];
    for label in &maps {
        args.push("-map".into());
        args.push(label.clone());
    }
    let Some(raw) = run_ffmpeg(&args, path.as_ref())? else {
        return Ok(None);
    };

    let mut out = SegmentSignals {
        motion: None,
        audio: Vec::new(),
    };
    if let Some(fps) = sample_fps {
        out.motion = fill(
            SignalTrack::new(
                SignalKind::Motion,
                "video",
                MediaTime::from_secs(1.0 / fps, HZ_1K)?,
            ),
            &raw,
            MOTION_KEY,
            None,
        );
    }
    let window = MediaTime::from_secs(window_secs.max(f64::MIN_POSITIVE), HZ_1K)?;
    for (slot, index) in audio_indices.iter().enumerate() {
        let Some(track) = fill(
            SignalTrack::new(SignalKind::AudioLevel, format!("audio:{index}"), window),
            &raw,
            &channel_rms_key(slot),
            Some(SILENCE_FLOOR_DB),
        ) else {
            // A partial audio result would silently drop a leg.
            return Ok(None);
        };
        out.audio.push(track);
    }
    if out.motion.is_none() && out.audio.is_empty() {
        return Ok(None);
    }
    Ok(Some(out))
}

/// `astats` numbers channels from 1; after the merge, slot `i` is channel `i + 1`.
fn channel_rms_key(slot: usize) -> String {
    format!("lavfi.astats.{}.RMS_level", slot + 1)
}

/// Filter graph plus the output labels to map.
fn measure_graph(
    sample_fps: Option<f64>,
    audio_indices: &[u32],
    window_secs: f64,
) -> (String, Vec<String>) {
    let mut chains: Vec<String> = Vec::new();
    let mut maps: Vec<String> = Vec::new();
    if let Some(fps) = sample_fps {
        chains.push(format!(
            "[0:v]fps={fps},scale=160:-2,signalstats,metadata=print:key={MOTION_KEY}:file=-[rfv]"
        ));
        maps.push("[rfv]".into());
    }
    if !audio_indices.is_empty() {
        let rate = 8_000.0_f64;
        let n = (rate * window_secs).round().max(1.0);
        let mut labels = String::new();
        let mut prints = String::new();
        for (slot, index) in audio_indices.iter().enumerate() {
            // Mono per leg keeps the channel numbering fixed regardless of
            // what the host device actually opened.
            chains.push(format!(
                "[0:a:{index}]aresample={rate:.0},aformat=channel_layouts=mono[rfa{slot}]"
            ));
            write!(labels, "[rfa{slot}]").expect("string write");
            write!(
                prints,
                ",ametadata=print:key={}:file=-",
                channel_rms_key(slot)
            )
            .expect("string write");
        }
        let merge = if audio_indices.len() > 1 {
            format!("{labels}amerge=inputs={},", audio_indices.len())
        } else {
            labels
        };
        chains.push(format!(
            "{merge}asetnsamples=n={n:.0}:p=0,astats=metadata=1:reset=1{prints}[rfa]"
        ));
        maps.push("[rfa]".into());
    }
    (chains.join(";"), maps)
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
    extract_audio_streams(src, &[(audio_index, dst.as_ref().to_path_buf())])
}

/// Demux several audio streams of one file in a single ffmpeg invocation.
///
/// ffmpeg accepts many outputs per input, so N legs cost one process and one
/// read instead of N. Either every target is written or the call fails —
/// callers that want per-leg isolation retry with [`extract_audio_stream`].
///
/// # Errors
///
/// Missing ffmpeg, a missing stream, or a non-zero exit (stderr is included).
pub fn extract_audio_streams(src: impl AsRef<Path>, targets: &[(u32, PathBuf)]) -> Result<()> {
    if targets.is_empty() {
        return Ok(());
    }
    let program = ffmpeg_program();
    let mut cmd = Command::new(&program);
    cmd.args(["-hide_banner", "-v", "error", "-y", "-i"])
        .arg(src.as_ref());
    for (audio_index, dst) in targets {
        cmd.args(["-map", &format!("0:a:{audio_index}"), "-vn", "-sn", "-dn"])
            .args(["-c", "copy"])
            .arg(dst);
    }
    let out = cmd
        .output()
        .map_err(|e| CaptureError::io(format!("{program} extract: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    let legs: Vec<String> = targets.iter().map(|(i, _)| format!("a:{i}")).collect();
    let why = String::from_utf8_lossy(&out.stderr);
    Err(CaptureError::io(format!(
        "extract {} from {}: {}",
        legs.join(", "),
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

pub(crate) fn ffmpeg_program() -> String {
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
        assert!(measure_segment("x.mkv", Some(0.0), &[], 0.5).is_err());
        assert!(measure_segment("x.mkv", None, &[0], 0.0).is_err());
        assert!(measure_segment("x.mkv", None, &[], 0.5).unwrap().is_none());
    }

    #[test]
    fn one_pass_graph_keeps_every_leg_on_its_own_key() {
        let (graph, maps) = measure_graph(Some(2.0), &[0, 1], 0.5);
        assert_eq!(maps, ["[rfv]", "[rfa]"]);
        assert!(graph.contains("[0:a:0]aresample=8000"), "{graph}");
        assert!(graph.contains("[0:a:1]aresample=8000"), "{graph}");
        assert!(graph.contains("[rfa0][rfa1]amerge=inputs=2"), "{graph}");
        assert!(graph.contains("asetnsamples=n=4000"), "{graph}");
        // Distinct keys are what makes the shared stdout parseable.
        assert!(graph.contains("key=lavfi.astats.1.RMS_level"), "{graph}");
        assert!(graph.contains("key=lavfi.astats.2.RMS_level"), "{graph}");
        assert!(graph.contains(MOTION_KEY), "{graph}");
    }

    #[test]
    fn a_single_leg_needs_no_merge() {
        let (graph, maps) = measure_graph(None, &[1], 0.5);
        assert_eq!(maps, ["[rfa]"]);
        assert!(!graph.contains("amerge"), "{graph}");
        assert!(graph.contains("[rfa0]asetnsamples"), "{graph}");
        assert!(graph.contains("key=lavfi.astats.1.RMS_level"), "{graph}");
    }

    #[test]
    fn motion_only_graph_has_no_audio_chain() {
        let (graph, maps) = measure_graph(Some(4.0), &[], 0.5);
        assert_eq!(maps, ["[rfv]"]);
        assert!(graph.contains("fps=4"), "{graph}");
        assert!(!graph.contains("astats"), "{graph}");
    }
}
