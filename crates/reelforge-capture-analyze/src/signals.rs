//! Measure committed segments and stitch the result onto the session clock.

use reelforge_capture_core::{AudioLeg, HZ_1K, MediaTime, Result, SignalKind, SignalTrack};
use reelforge_capture_platform::{audio_level_series, measure_segment, motion_series};
use reelforge_capture_store::SessionStore;

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
/// Each segment is walked **once**: picture and every audio leg come out of a
/// single ffmpeg pass ([`measure_segment`]). If that pass cannot read a
/// segment — an old ffmpeg, a leg the device never opened — the signals are
/// measured one at a time instead, so one broken stream does not cost the
/// others.
///
/// Per-segment measurements are offset onto the session clock, so a track
/// spans the whole recording. Sources the host could not measure are simply
/// absent — callers must not read that as quiet.
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

    // Legs are measured off the muxed segment by stream index: the demuxed
    // per-leg file is a bit-for-bit copy, so it carries no extra information
    // and would cost another decode pass.
    let legs: Vec<(AudioLeg, u32)> = if opts.audio {
        mix.configured()
            .iter()
            .filter_map(|(leg, _)| mix.audio_index(*leg).map(|i| (*leg, i)))
            .collect()
    } else {
        Vec::new()
    };
    if !opts.motion && legs.is_empty() {
        return Ok(Vec::new());
    }

    let mut motion = SignalTrack::new(
        SignalKind::Motion,
        "video",
        MediaTime::from_secs(1.0 / opts.sample_fps.max(f64::MIN_POSITIVE), HZ_1K)?,
    );
    let window = MediaTime::from_secs(opts.audio_window_secs.max(f64::MIN_POSITIVE), HZ_1K)?;
    let mut audio: Vec<SignalTrack> = legs
        .iter()
        .map(|(leg, _)| {
            SignalTrack::new(
                SignalKind::AudioLevel,
                format!("audio:{}", leg.as_str()),
                window,
            )
        })
        .collect();

    let indices: Vec<u32> = legs.iter().map(|(_, i)| *i).collect();
    let fps = opts.motion.then_some(opts.sample_fps);

    for seg in &segments {
        let file = store.root().join(&seg.path);
        if let Some(measured) = measure_segment(&file, fps, &indices, opts.audio_window_secs)? {
            if let Some(track) = measured.motion {
                append_offset(&mut motion, &track, seg.start.ticks);
            }
            for (slot, track) in measured.audio.iter().enumerate() {
                if let Some(dst) = audio.get_mut(slot) {
                    append_offset(dst, track, seg.start.ticks);
                }
            }
            continue;
        }
        // One pass could not read this segment; salvage what is readable.
        if opts.motion
            && let Some(measured) = motion_series(&file, opts.sample_fps)?
        {
            append_offset(&mut motion, &measured, seg.start.ticks);
        }
        for (slot, (_, index)) in legs.iter().enumerate() {
            let Some(dst) = audio.get_mut(slot) else {
                continue;
            };
            if let Some(measured) =
                audio_level_series(&file, *index, opts.audio_window_secs, &dst.source)?
            {
                append_offset(dst, &measured, seg.start.ticks);
            }
        }
    }

    let mut out = Vec::new();
    if motion.is_measured() {
        out.push(motion);
    }
    out.extend(audio.into_iter().filter(SignalTrack::is_measured));
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
