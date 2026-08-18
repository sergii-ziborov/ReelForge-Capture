//! Per-segment A/V clock decision (video master, session fallback).
//!
//! The supervisor **slews** the live session clock to the decided out-point
//! after each commit so pointer events stay on the media timeline. This is
//! not a packet-level `itsoffset` rewrite.

use reelforge_capture_core::{MediaTime, Result, SegmentId};
use reelforge_capture_platform::{MediaClocks, probe_media_clocks};
use reelforge_capture_store::{ClockAudioLeg, ClockMaster, ClockSegment, SessionStore};

/// One audio stream in a [`ClockSample`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioClockSample {
    /// `a:N`.
    pub index: u32,
    /// Stream duration.
    pub duration: Option<MediaTime>,
    /// Stream `start_time`.
    pub start: Option<MediaTime>,
}

/// Probed + wall measurements at the moment a segment is committed.
#[derive(Debug, Clone, PartialEq)]
pub struct ClockSample {
    /// Session clock at commit (pauses excluded).
    pub now: MediaTime,
    /// Video-stream duration (not the container, when they disagree).
    pub video: Option<MediaTime>,
    /// First video `start_time`.
    pub video_start: Option<MediaTime>,
    /// Every audio stream.
    pub audio: Vec<AudioClockSample>,
}

impl ClockSample {
    /// Build from a live session time and one ffprobe snapshot.
    #[must_use]
    pub fn from_probed(now: MediaTime, probed: &MediaClocks) -> Self {
        Self {
            now,
            video: probed.video_master(),
            video_start: probed.video_start,
            audio: probed
                .audio
                .iter()
                .map(|a| AudioClockSample {
                    index: a.index,
                    duration: a.duration,
                    start: a.start,
                })
                .collect(),
        }
    }
}

/// How this segment lands on the session timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct ClockDecision {
    /// Session-clock out-point.
    pub end: MediaTime,
    /// Duration that owns `end`.
    pub master: ClockMaster,
    /// Wall / monotonic span of this segment (0 if the clock had not moved).
    pub session_secs: f64,
    /// Probed video seconds.
    pub video_secs: Option<f64>,
    /// First audio stream seconds (compat / summary).
    pub audio_secs: Option<f64>,
    /// First video `start_time`.
    pub video_start_secs: Option<f64>,
    /// Every audio stream.
    pub audio: Vec<ClockAudioLeg>,
    /// `|master − expected|` exceeded the gap threshold.
    pub frame_gap: bool,
    /// Any audio stream drifted from the video master.
    pub audio_gap: bool,
    /// `master_secs − session_secs` in milliseconds.
    pub correction_ms: i64,
}

/// Choose the segment out-point and whether to emit gap events.
///
/// Video probe wins. Without it the session clock is the timeline. Audio
/// never owns the timeline — a short/long audio stream is an `audio_gap`
/// for the project to turn into a timeline gap later, not a stretch of
/// the picture.
#[must_use]
pub fn decide_clocks(
    start: MediaTime,
    sample: &ClockSample,
    budget_secs: f64,
    gap_secs: f64,
) -> ClockDecision {
    let session_secs = if sample.now.ticks > start.ticks {
        sample.now.as_secs() - start.as_secs()
    } else {
        0.0
    };
    let budget = budget_secs.max(0.0);

    let scale = start.timescale.max(1);
    let (end, master, video_secs) = if let Some(dur) = sample.video.filter(|d| d.ticks > 0) {
        (
            MediaTime {
                ticks: start.ticks.saturating_add(dur.ticks),
                timescale: scale,
            },
            ClockMaster::Video,
            Some(dur.as_secs()),
        )
    } else if sample.now.ticks > start.ticks {
        (sample.now, ClockMaster::Session, None)
    } else {
        (
            MediaTime {
                ticks: start.ticks.saturating_add(1),
                timescale: scale,
            },
            ClockMaster::Session,
            None,
        )
    };

    let actual = (end.as_secs() - start.as_secs()).max(0.0);
    let audio: Vec<ClockAudioLeg> = sample
        .audio
        .iter()
        .map(|a| ClockAudioLeg {
            index: a.index,
            duration_secs: a.duration.filter(|d| d.ticks > 0).map(MediaTime::as_secs),
            start_secs: a.start.filter(|d| d.ticks > 0).map(MediaTime::as_secs),
        })
        .collect();
    let audio_secs = audio.iter().find_map(|a| a.duration_secs);
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let correction_ms = ((actual - session_secs) * 1_000.0).round() as i64;

    // A gap needs two clocks. Instant commit of an unreadable file is not
    // drift — we simply have no measurement to compare.
    let frame_gap = match (video_secs, session_secs > 0.0) {
        (Some(video), true) => (video - session_secs).abs() > gap_secs,
        (Some(video), false) => (video - budget).abs() > gap_secs,
        (None, true) => (session_secs - budget).abs() > gap_secs,
        (None, false) => false,
    };
    let audio_gap = audio
        .iter()
        .filter_map(|a| a.duration_secs)
        .any(|secs| (secs - actual).abs() > gap_secs);

    ClockDecision {
        end,
        master,
        session_secs,
        video_secs,
        audio_secs,
        video_start_secs: sample
            .video_start
            .filter(|d| d.ticks > 0)
            .map(MediaTime::as_secs),
        audio,
        frame_gap,
        audio_gap,
        correction_ms,
    }
}

/// Persist a clock row from a decision (live commit or repair).
#[must_use]
pub fn clock_row(id: SegmentId, start: MediaTime, decision: &ClockDecision) -> ClockSegment {
    ClockSegment {
        id,
        start,
        end: decision.end,
        master: decision.master,
        session_secs: decision.session_secs,
        video_secs: decision.video_secs,
        audio_secs: decision.audio_secs,
        video_start_secs: decision.video_start_secs,
        audio: decision.audio.clone(),
        correction_ms: decision.correction_ms,
    }
}

/// Backfill `clocks.json` for committed segments that have no row.
///
/// Does **not** rewrite the manifest. Used after a crash or for sessions
/// captured before clocks were recorded. Returns how many rows were added.
///
/// # Errors
///
/// Store I/O.
pub fn repair_clocks(store: &SessionStore) -> Result<usize> {
    let existing = store.read_clocks()?.unwrap_or_default();
    let budget = store.manifest().meta.spec.segment_secs;
    let mut added = 0usize;
    for seg in &store.manifest().segments {
        if existing.segment(seg.id).is_some() {
            continue;
        }
        let probed = probe_media_clocks(store.root().join(&seg.path)).unwrap_or_default();
        // Recorded span is the only session clock we still have.
        let decision = decide_clocks(
            seg.start,
            &ClockSample::from_probed(seg.end, &probed),
            budget,
            0.75,
        );
        // Keep the committed out-point; only fill the measurement row.
        let mut row = clock_row(seg.id, seg.start, &decision);
        row.end = seg.end;
        store.append_clock(row)?;
        added = added.saturating_add(1);
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelforge_capture_core::{HZ_1K, SegmentId};

    fn t(secs: f64) -> MediaTime {
        MediaTime::from_secs(secs, HZ_1K).unwrap()
    }

    #[test]
    fn video_probe_owns_the_timeline() {
        let d = decide_clocks(
            t(10.0),
            &ClockSample {
                now: t(15.2),
                video: Some(t(5.0)),
                video_start: None,
                audio: vec![AudioClockSample {
                    index: 0,
                    duration: Some(t(4.94)),
                    start: Some(t(0.021)),
                }],
            },
            5.0,
            0.75,
        );
        assert_eq!(d.master, ClockMaster::Video);
        assert!((d.end.as_secs() - 15.0).abs() < 1e-9);
        assert_eq!(d.correction_ms, -200);
        assert!(!d.frame_gap);
        assert!(!d.audio_gap);
    }

    #[test]
    fn wall_lead_over_threshold_is_a_frame_gap() {
        let d = decide_clocks(
            t(0.0),
            &ClockSample {
                now: t(6.0),
                video: Some(t(5.0)),
                video_start: None,
                audio: Vec::new(),
            },
            5.0,
            0.75,
        );
        assert!(d.frame_gap);
        assert_eq!(d.correction_ms, -1000);
    }

    #[test]
    fn short_audio_is_an_audio_gap_not_a_timeline_owner() {
        let d = decide_clocks(
            t(0.0),
            &ClockSample {
                now: t(5.0),
                video: Some(t(5.0)),
                video_start: None,
                audio: vec![AudioClockSample {
                    index: 0,
                    duration: Some(t(4.0)),
                    start: None,
                }],
            },
            5.0,
            0.75,
        );
        assert_eq!(d.master, ClockMaster::Video);
        assert!(d.audio_gap);
        assert!((d.end.as_secs() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn second_audio_leg_can_trip_the_gap() {
        let d = decide_clocks(
            t(0.0),
            &ClockSample {
                now: t(5.0),
                video: Some(t(5.0)),
                video_start: None,
                audio: vec![
                    AudioClockSample {
                        index: 0,
                        duration: Some(t(5.0)),
                        start: None,
                    },
                    AudioClockSample {
                        index: 1,
                        duration: Some(t(3.8)),
                        start: Some(t(0.02)),
                    },
                ],
            },
            5.0,
            0.75,
        );
        assert!(d.audio_gap);
        assert_eq!(d.audio.len(), 2);
        assert_eq!(d.audio[1].start_secs, Some(0.02));
        assert!(!d.frame_gap);
    }

    #[test]
    fn missing_probe_falls_back_to_the_session_clock() {
        let d = decide_clocks(
            t(0.0),
            &ClockSample {
                now: t(5.0),
                video: None,
                video_start: None,
                audio: Vec::new(),
            },
            5.0,
            0.75,
        );
        assert_eq!(d.master, ClockMaster::Session);
        assert_eq!(d.end, t(5.0));
        assert_eq!(d.correction_ms, 0);
        assert!(!d.frame_gap);
    }

    #[test]
    fn clock_not_moved_uses_budget_and_one_tick() {
        let d = decide_clocks(
            t(0.0),
            &ClockSample {
                now: t(0.0),
                video: None,
                video_start: None,
                audio: Vec::new(),
            },
            5.0,
            0.75,
        );
        assert_eq!(d.master, ClockMaster::Session);
        assert_eq!(d.end.ticks, 1);
        assert!(
            !d.frame_gap,
            "1 ms vs 5 s budget is a missing probe, not drift"
        );
    }

    #[test]
    fn repair_fills_missing_rows_without_rewriting_the_manifest() {
        use reelforge_capture_core::{CaptureSpec, SessionId, SessionMeta};
        use reelforge_capture_store::{SegmentRecord, SessionStore};
        use std::time::{SystemTime, UNIX_EPOCH};

        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rf-clk-fix-{n}"));
        let mut store = SessionStore::create(
            &root,
            SessionMeta {
                id: SessionId::new("ses_fix"),
                name: "fix".into(),
                spec: CaptureSpec::screen(),
                started_unix: None,
                duration: None,
            },
        )
        .unwrap();
        store
            .commit_segment(SegmentRecord {
                id: SegmentId(1),
                path: "segments/000001.mkv".into(),
                start: t(0.0),
                end: t(5.0),
            })
            .unwrap();
        assert_eq!(repair_clocks(&store).unwrap(), 1);
        assert_eq!(repair_clocks(&store).unwrap(), 0);
        let row = store.read_clocks().unwrap().unwrap().segments[0].clone();
        assert_eq!(row.id, SegmentId(1));
        assert_eq!(row.end, t(5.0));
        let _ = std::fs::remove_dir_all(root);
    }
}
