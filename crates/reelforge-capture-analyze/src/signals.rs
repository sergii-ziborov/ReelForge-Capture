//! Measure committed segments and stitch the result onto the session clock.

use reelforge_capture_core::{AudioLeg, HZ_1K, MediaTime, Result, SignalKind, SignalTrack};
use reelforge_capture_platform::{audio_level_series, motion_series};
use reelforge_capture_store::SessionStore;
use std::path::PathBuf;

/// What to measure and how finely.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalOptions {
    /// Measure frame difference on the video stream.
    pub motion: bool,
    /// Measure audio energy on each configured leg.
    pub audio: bool,
    /// Frame-difference samples per second.
    pub sample_fps: f64,
    /// Audio energy window in seconds.
    pub audio_window_secs: f64,
}

impl Default for SignalOptions {
    fn default() -> Self {
        Self {
            motion: true,
            audio: true,
            sample_fps: 2.0,
            audio_window_secs: 0.5,
        }
    }
}

/// Measure every committed segment and return one track per signal source.
///
/// Per-segment measurements are offset onto the session clock, so a track
/// spans the whole recording. Sources the host could not measure (no ffmpeg,
/// no such stream) are simply absent — callers must not read that as quiet.
///
/// # Errors
///
/// Invalid options, or host I/O other than a missing ffmpeg.
pub fn session_signals(store: &SessionStore, opts: &SignalOptions) -> Result<Vec<SignalTrack>> {
    let segments = store.manifest().segments.clone();
    if segments.is_empty() {
        return Ok(Vec::new());
    }
    let mix = store.manifest().meta.spec.audio.clone();
    let sidecar = store.read_audio_sidecar()?;

    let mut motion = SignalTrack::new(
        SignalKind::Motion,
        "video",
        MediaTime::from_secs(1.0 / opts.sample_fps.max(f64::MIN_POSITIVE), HZ_1K)?,
    );
    let mut legs: Vec<(AudioLeg, SignalTrack)> = mix
        .configured()
        .iter()
        .map(|(leg, _)| {
            (
                *leg,
                SignalTrack::new(
                    SignalKind::AudioLevel,
                    format!("audio:{}", leg.as_str()),
                    MediaTime::from_secs(opts.audio_window_secs, HZ_1K)
                        .unwrap_or_else(|_| MediaTime::zero(HZ_1K)),
                ),
            )
        })
        .collect();

    for seg in &segments {
        let file = store.root().join(&seg.path);
        if opts.motion
            && let Some(measured) = motion_series(&file, opts.sample_fps)?
        {
            append_offset(&mut motion, &measured, seg.start.ticks);
        }
        if !opts.audio {
            continue;
        }
        for (leg, track) in &mut legs {
            // A demuxed leg file is single-stream; the muxed segment is not.
            let demuxed = sidecar
                .as_ref()
                .and_then(|s| s.leg(*leg))
                .and_then(|l| l.files.iter().find(|f| f.segment == seg.id))
                .map(|f| store.root().join(&f.path))
                .filter(|p| p.is_file());
            let (path, index): (PathBuf, u32) = match demuxed {
                Some(p) => (p, 0),
                None => match mix.audio_index(*leg) {
                    Some(i) => (file.clone(), i),
                    None => continue,
                },
            };
            if let Some(measured) =
                audio_level_series(&path, index, opts.audio_window_secs, &track.source)?
            {
                append_offset(track, &measured, seg.start.ticks);
            }
        }
    }

    let mut out = Vec::new();
    if motion.is_measured() {
        out.push(motion);
    }
    out.extend(
        legs.into_iter()
            .map(|(_, t)| t)
            .filter(SignalTrack::is_measured),
    );
    Ok(out)
}

/// Append `src` to `dst`, shifting sample times onto the session clock.
fn append_offset(dst: &mut SignalTrack, src: &SignalTrack, offset_ticks: i64) {
    for s in &src.samples {
        dst.push(
            MediaTime {
                ticks: s.t.ticks.saturating_add(offset_ticks),
                timescale: dst.period.timescale.max(1),
            },
            s.value,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: f64) -> MediaTime {
        MediaTime::from_secs(secs, HZ_1K).unwrap()
    }

    #[test]
    fn segment_measurements_land_on_the_session_clock() {
        let mut first = SignalTrack::new(SignalKind::Motion, "video", t(0.5));
        first.push(t(0.0), 1.0);
        first.push(t(0.5), 2.0);

        let mut session = SignalTrack::new(SignalKind::Motion, "video", t(0.5));
        append_offset(&mut session, &first, 0);
        append_offset(&mut session, &first, t(5.0).ticks);

        assert_eq!(session.samples.len(), 4);
        assert!((session.samples[2].t.as_secs() - 5.0).abs() < 1e-9);
        assert!((session.samples[3].t.as_secs() - 5.5).abs() < 1e-9);
        let coverage = session.coverage().unwrap();
        assert!((coverage.end.as_secs() - 6.0).abs() < 1e-9);
    }

    #[test]
    fn defaults_measure_both_signals() {
        let opts = SignalOptions::default();
        assert!(opts.motion && opts.audio);
        assert!((opts.sample_fps - 2.0).abs() < 1e-9);
    }
}
