//! Idle detection.
//!
//! Two levels:
//!
//! * [`detect_idle`] — pointer-only (no move, no click). Cheap, and the only
//!   signal available while recording.
//! * [`detect_idle_multi`] — agreement between every measured signal:
//!   pointer, frame difference, and audio energy. A range is idle only if
//!   *every* source that has evidence there says so.
//!
//! Missing evidence is never idle: an unmeasured source contributes nothing,
//! and if no source has evidence the result is empty. That is what keeps
//! `idle --remove` from proposing "delete the whole session".

use reelforge_capture_core::{
    MediaRange, MediaTime, PointerEvent, Result, SignalKind, SignalTrack,
};

/// Thresholds for [`detect_idle_multi`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IdleConfig {
    /// Minimum length of an idle range.
    pub threshold: MediaTime,
    /// Frame difference above this counts as picture activity (`YDIF` units).
    pub motion_above: f64,
    /// Audio RMS above this counts as sound activity (dBFS).
    pub audio_above_db: f64,
}

impl IdleConfig {
    /// Defaults tuned for screen recordings: a cursor blink or codec noise
    /// stays under `motion_above`; room tone stays under `audio_above_db`.
    #[must_use]
    pub const fn new(threshold: MediaTime) -> Self {
        Self {
            threshold,
            motion_above: 1.0,
            audio_above_db: -45.0,
        }
    }
}

/// What [`detect_idle_multi`] concluded, and from which evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct IdleReport {
    /// Ranges every measured source agrees are idle.
    pub ranges: Vec<MediaRange>,
    /// Labels of the sources that voted (`pointer`, `motion:video`, …).
    pub sources: Vec<String>,
}

impl IdleReport {
    /// No source had evidence.
    #[must_use]
    pub fn is_blind(&self) -> bool {
        self.sources.is_empty()
    }
}

/// Detect stretches where the cursor is still and nobody clicks.
///
/// `threshold` is the minimum still time. The last sample is held until
/// `duration` (session end). No pointer samples → no idle ranges.
///
/// # Errors
///
/// Invalid range construction.
pub fn detect_idle(
    events: &[PointerEvent],
    duration: MediaTime,
    threshold: MediaTime,
) -> Result<Vec<MediaRange>> {
    let scale = duration.timescale.max(1);
    let need = threshold.as_secs();
    if need <= 0.0 || duration.ticks <= 0 {
        return Ok(Vec::new());
    }

    let mut marks: Vec<(f64, i32, i32, bool)> = Vec::new();
    for e in events {
        let Some((x, y)) = e.position() else {
            continue;
        };
        marks.push((e.time().as_secs(), x, y, e.is_click()));
    }
    marks.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let end_s = duration.as_secs();
    // Absence of pointer evidence is unknown, not idle.
    if marks.is_empty() {
        return Ok(Vec::new());
    }

    let mut idle = Vec::new();
    // Do not invent idle before the first sample — the collector may have started late.
    let mut last_t = marks[0].0;
    let mut last_xy = marks[0];

    for m in marks
        .iter()
        .skip(1)
        .copied()
        .chain(std::iter::once((end_s, last_xy.1, last_xy.2, false)))
    {
        let dt = m.0 - last_t;
        let moved = m.1 != last_xy.1 || m.2 != last_xy.2;
        let click = last_xy.3 || m.3;
        if dt + 1e-9 >= need && !moved && !click {
            idle.push(MediaRange::new(
                MediaTime::from_secs(last_t, scale)?,
                MediaTime::from_secs(m.0, scale)?,
            )?);
        }
        if m.0 < end_s || marks.len() == 1 {
            last_t = m.0;
            last_xy = m;
        } else {
            last_t = m.0;
        }
    }
    Ok(idle)
}

/// Stretches of a measured signal that stay at or below `above`.
///
/// Only the span the samples actually cover is considered, and each active
/// sample silences a full period on both sides — an unmeasured or noisy edge
/// shrinks the quiet range instead of extending it.
///
/// # Errors
///
/// Invalid range construction.
pub fn quiet_ranges(
    track: &SignalTrack,
    above: f64,
    duration: MediaTime,
) -> Result<Vec<MediaRange>> {
    let Some(coverage) = track.coverage() else {
        return Ok(Vec::new());
    };
    let scale = duration.timescale.max(1);
    let end_cap = if duration.ticks > 0 {
        duration.ticks
    } else {
        coverage.end.ticks
    };
    let mut quiet = vec![Span {
        start: coverage.start.ticks,
        end: coverage.end.ticks.min(end_cap),
    }];
    let period = track.period.ticks.max(1);
    for s in &track.samples {
        if s.value <= above {
            continue;
        }
        quiet = subtract(
            &quiet,
            Span {
                start: s.t.ticks - period,
                end: s.t.ticks + period,
            },
        );
    }
    spans_to_ranges(&quiet, scale)
}

/// Idle ranges every measured source agrees on.
///
/// Sources that have no evidence do not vote (and do not veto). With no
/// evidence at all the report is blind and carries no ranges.
///
/// # Errors
///
/// Invalid range construction.
pub fn detect_idle_multi(
    events: &[PointerEvent],
    tracks: &[SignalTrack],
    duration: MediaTime,
    config: IdleConfig,
) -> Result<IdleReport> {
    let scale = duration.timescale.max(1);
    let mut votes: Vec<Vec<Span>> = Vec::new();
    let mut sources: Vec<String> = Vec::new();

    if events.iter().any(PointerEvent::is_pointer) {
        votes.push(to_spans(&detect_idle(events, duration, config.threshold)?));
        sources.push("pointer".into());
    }
    for track in tracks {
        if !track.is_measured() {
            continue;
        }
        let above = match track.kind {
            SignalKind::Motion => config.motion_above,
            SignalKind::AudioLevel => config.audio_above_db,
        };
        votes.push(to_spans(&quiet_ranges(track, above, duration)?));
        sources.push(track.label());
    }

    if votes.is_empty() {
        return Ok(IdleReport {
            ranges: Vec::new(),
            sources,
        });
    }

    let mut agreed = votes.swap_remove(0);
    for other in &votes {
        agreed = intersect(&agreed, other);
    }
    let min = config.threshold.ticks.max(1);
    agreed.retain(|s| s.end - s.start >= min);
    Ok(IdleReport {
        ranges: spans_to_ranges(&agreed, scale)?,
        sources,
    })
}

/// Half-open tick interval; simpler than [`MediaRange`] for set algebra
/// (empty results are normal here, not an error).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: i64,
    end: i64,
}

fn to_spans(ranges: &[MediaRange]) -> Vec<Span> {
    ranges
        .iter()
        .map(|r| Span {
            start: r.start.ticks,
            end: r.end.ticks,
        })
        .collect()
}

fn spans_to_ranges(spans: &[Span], scale: u32) -> Result<Vec<MediaRange>> {
    let mut out = Vec::new();
    for s in spans {
        if s.end <= s.start {
            continue;
        }
        out.push(MediaRange::new(
            MediaTime {
                ticks: s.start,
                timescale: scale,
            },
            MediaTime {
                ticks: s.end,
                timescale: scale,
            },
        )?);
    }
    Ok(out)
}

fn subtract(spans: &[Span], cut: Span) -> Vec<Span> {
    let mut out = Vec::new();
    for s in spans {
        if cut.end <= s.start || cut.start >= s.end {
            out.push(*s);
            continue;
        }
        if cut.start > s.start {
            out.push(Span {
                start: s.start,
                end: cut.start,
            });
        }
        if cut.end < s.end {
            out.push(Span {
                start: cut.end,
                end: s.end,
            });
        }
    }
    out
}

fn intersect(a: &[Span], b: &[Span]) -> Vec<Span> {
    let mut out = Vec::new();
    for x in a {
        for y in b {
            let start = x.start.max(y.start);
            let end = x.end.min(y.end);
            if end > start {
                out.push(Span { start, end });
            }
        }
    }
    out.sort_by_key(|s| s.start);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelforge_capture_core::{HZ_1K, PointerEvent};

    fn t(secs: f64) -> MediaTime {
        MediaTime::from_secs(secs, HZ_1K).unwrap()
    }

    fn cur(s: f64, x: i32, y: i32) -> PointerEvent {
        PointerEvent::Cursor { t: t(s), x, y }
    }

    fn track(kind: SignalKind, period: f64, values: &[f64]) -> SignalTrack {
        let mut track = SignalTrack::new(kind, "video", t(period));
        for (i, v) in values.iter().enumerate() {
            let i = u32::try_from(i).expect("small test fixture");
            track.push(t(f64::from(i) * period), *v);
        }
        track
    }

    #[test]
    fn still_cursor_is_idle() {
        let ev = vec![cur(0.0, 1, 1), cur(4.0, 1, 1)];
        let idle = detect_idle(&ev, t(4.0), t(2.0)).unwrap();
        assert_eq!(idle.len(), 1);
        assert!((idle[0].duration_secs() - 4.0).abs() < 1e-9);
    }

    #[test]
    fn empty_log_is_not_idle() {
        let idle = detect_idle(&[], t(30.0), t(3.0)).unwrap();
        assert!(idle.is_empty());
    }

    #[test]
    fn motion_breaks_idle() {
        let ev = vec![cur(0.0, 0, 0), cur(1.0, 50, 0), cur(2.0, 50, 0)];
        let idle = detect_idle(&ev, t(2.0), t(1.5)).unwrap();
        assert!(idle.is_empty());
    }

    #[test]
    fn quiet_signal_covers_only_measured_time() {
        // 10 samples of 0.5 s from t=0 → coverage [0, 5).
        let flat = track(SignalKind::Motion, 0.5, &[0.0; 10]);
        let quiet = quiet_ranges(&flat, 1.0, t(30.0)).unwrap();
        assert_eq!(quiet.len(), 1);
        assert!((quiet[0].start.as_secs() - 0.0).abs() < 1e-9);
        assert!(
            (quiet[0].end.as_secs() - 5.0).abs() < 1e-9,
            "must not claim the unmeasured tail: {:?}",
            quiet[0]
        );
    }

    #[test]
    fn a_loud_sample_silences_a_period_each_side() {
        let mut motion = track(SignalKind::Motion, 0.5, &[0.0; 10]);
        motion.samples[4].value = 20.0; // activity at t = 2.0
        let quiet = quiet_ranges(&motion, 1.0, t(5.0)).unwrap();
        assert_eq!(quiet.len(), 2, "{quiet:?}");
        assert!((quiet[0].end.as_secs() - 1.5).abs() < 1e-9);
        assert!((quiet[1].start.as_secs() - 2.5).abs() < 1e-9);
    }

    #[test]
    fn unmeasured_track_says_nothing() {
        let empty = SignalTrack::new(SignalKind::AudioLevel, "audio:system", t(0.5));
        assert!(quiet_ranges(&empty, -45.0, t(10.0)).unwrap().is_empty());
        let report = detect_idle_multi(
            &[],
            std::slice::from_ref(&empty),
            t(10.0),
            IdleConfig::new(t(2.0)),
        )
        .unwrap();
        assert!(report.is_blind());
        assert!(report.ranges.is_empty());
    }

    #[test]
    fn audio_energy_vetoes_a_still_cursor() {
        let ev = vec![cur(0.0, 1, 1), cur(5.0, 1, 1)];
        let mut audio = SignalTrack::new(SignalKind::AudioLevel, "audio:microphone", t(0.5));
        for i in 0..10 {
            let at = f64::from(i) * 0.5;
            // Talking over the still cursor from 2 s on.
            audio.push(t(at), if at >= 2.0 { -20.0 } else { -90.0 });
        }
        let cfg = IdleConfig::new(t(1.0));
        let pointer_only = detect_idle(&ev, t(5.0), cfg.threshold).unwrap();
        assert_eq!(pointer_only.len(), 1, "pointer alone calls it idle");

        let report = detect_idle_multi(&ev, &[audio], t(5.0), cfg).unwrap();
        assert_eq!(report.sources, ["pointer", "audio_level:audio:microphone"]);
        assert_eq!(report.ranges.len(), 1);
        assert!(
            report.ranges[0].end.as_secs() <= 1.5 + 1e-9,
            "speech must cut the idle range short: {:?}",
            report.ranges[0]
        );
    }

    #[test]
    fn frame_difference_vetoes_a_parked_cursor() {
        // Cursor parked all session, but the screen keeps changing (a video plays).
        let ev = vec![cur(0.0, 1, 1), cur(5.0, 1, 1)];
        let busy = track(SignalKind::Motion, 0.5, &[8.0; 10]);
        let report = detect_idle_multi(&ev, &[busy], t(5.0), IdleConfig::new(t(1.0))).unwrap();
        assert!(report.ranges.is_empty(), "{:?}", report.ranges);
        assert_eq!(report.sources.len(), 2);
    }

    #[test]
    fn agreement_keeps_the_quiet_overlap() {
        let ev = vec![cur(0.0, 1, 1), cur(6.0, 1, 1)];
        let motion = track(SignalKind::Motion, 0.5, &[0.0; 12]);
        let audio = {
            let mut a = SignalTrack::new(SignalKind::AudioLevel, "audio:system", t(0.5));
            for i in 0..12 {
                a.push(t(f64::from(i) * 0.5), -95.0);
            }
            a
        };
        let report =
            detect_idle_multi(&ev, &[motion, audio], t(6.0), IdleConfig::new(t(2.0))).unwrap();
        assert_eq!(report.ranges.len(), 1);
        assert!((report.ranges[0].start.as_secs() - 0.0).abs() < 1e-9);
        assert!((report.ranges[0].end.as_secs() - 6.0).abs() < 1e-9);
        assert_eq!(report.sources.len(), 3);
    }

    #[test]
    fn ranges_shorter_than_the_threshold_are_dropped() {
        let ev = vec![cur(0.0, 1, 1), cur(1.0, 1, 1)];
        let report = detect_idle_multi(&ev, &[], t(1.0), IdleConfig::new(t(5.0))).unwrap();
        assert!(report.ranges.is_empty());
        assert_eq!(report.sources, ["pointer"]);
    }
}
