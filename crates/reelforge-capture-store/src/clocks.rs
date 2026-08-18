//! `clocks.json` — per-segment session / video / audio measurements.
//!
//! The supervisor writes one row on each commit. Host and the project do not
//! re-probe to learn how far the wall clock drifted from the media.

use crate::SessionStore;
use reelforge_capture_core::{MediaTime, Result, SegmentId};
use serde::{Deserialize, Serialize};
use std::fs;

/// Schema of [`ClockSidecar`].
pub const CLOCK_SIDECAR_VERSION: u32 = 1;

/// Which duration owns the session timeline for this segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockMaster {
    /// Probed video / container duration.
    Video,
    /// Session (wall / monotonic) elapsed, when no probe was available.
    Session,
}

impl ClockMaster {
    /// Wire label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Session => "session",
        }
    }
}

/// One audio stream on a committed segment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClockAudioLeg {
    /// `a:N` among audio streams.
    pub index: u32,
    /// Probed duration, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
    /// Stream `start_time` (device / AAC priming).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_secs: Option<f64>,
}

/// One committed segment's clocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClockSegment {
    /// Segment ordinal.
    pub id: SegmentId,
    /// Session-clock in-point.
    pub start: MediaTime,
    /// Session-clock out-point (`start` + master duration).
    pub end: MediaTime,
    /// Master used for `end`.
    pub master: ClockMaster,
    /// Wall / monotonic span at commit (seconds).
    pub session_secs: f64,
    /// Probed video duration, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_secs: Option<f64>,
    /// Probed first-audio-stream duration, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_secs: Option<f64>,
    /// First video `start_time`, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_start_secs: Option<f64>,
    /// Every audio stream (`a:0`, `a:1`, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio: Vec<ClockAudioLeg>,
    /// `media_secs − session_secs` in milliseconds (negative = media shorter).
    pub correction_ms: i64,
}

/// `clocks.json` contents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClockSidecar {
    /// [`CLOCK_SIDECAR_VERSION`].
    pub version: u32,
    /// One row per committed segment, record order.
    #[serde(default)]
    pub segments: Vec<ClockSegment>,
}

impl ClockSidecar {
    /// Empty sidecar at the current version.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            version: CLOCK_SIDECAR_VERSION,
            segments: Vec::new(),
        }
    }

    /// Row for one segment.
    #[must_use]
    pub fn segment(&self, id: SegmentId) -> Option<&ClockSegment> {
        self.segments.iter().find(|s| s.id == id)
    }
}

impl ClockSegment {
    /// Audio stream `a:index`.
    #[must_use]
    pub fn audio_leg(&self, index: u32) -> Option<&ClockAudioLeg> {
        self.audio.iter().find(|a| a.index == index)
    }
}

impl Default for ClockSidecar {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore {
    /// Path of `clocks.json`.
    #[must_use]
    pub fn clocks_path(&self) -> std::path::PathBuf {
        self.root.join("clocks.json")
    }

    /// Read `clocks.json` (`None` when never written).
    ///
    /// # Errors
    ///
    /// I/O / JSON.
    pub fn read_clocks(&self) -> Result<Option<ClockSidecar>> {
        let path = self.clocks_path();
        if !path.is_file() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&fs::read_to_string(path)?)?))
    }

    /// Write `clocks.json` atomically.
    ///
    /// # Errors
    ///
    /// I/O / JSON.
    pub fn write_clocks(&self, sidecar: &ClockSidecar) -> Result<()> {
        let dest = self.clocks_path();
        let tmp = self.root.join("clocks.json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(sidecar)?)?;
        let _ = fs::remove_file(&dest);
        fs::rename(tmp, dest)?;
        Ok(())
    }

    /// Insert or replace one segment row and persist.
    ///
    /// # Errors
    ///
    /// I/O / JSON.
    pub fn append_clock(&self, rec: ClockSegment) -> Result<()> {
        let mut side = self.read_clocks()?.unwrap_or_default();
        if let Some(existing) = side.segments.iter_mut().find(|s| s.id == rec.id) {
            *existing = rec;
        } else {
            side.segments.push(rec);
        }
        self.write_clocks(&side)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionStore;
    use reelforge_capture_core::{CaptureSpec, HZ_1K, SessionId, SessionMeta};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn t(secs: f64) -> MediaTime {
        MediaTime::from_secs(secs, HZ_1K).unwrap()
    }

    #[test]
    fn round_trips_and_replaces_same_id() {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rf-clk-{n}"));
        let store = SessionStore::create(
            &root,
            SessionMeta {
                id: SessionId::new("ses_c"),
                name: "c".into(),
                spec: CaptureSpec::screen(),
                started_unix: None,
                duration: None,
            },
        )
        .unwrap();
        store
            .append_clock(ClockSegment {
                id: SegmentId(1),
                start: t(0.0),
                end: t(5.0),
                master: ClockMaster::Video,
                session_secs: 5.2,
                video_secs: Some(5.0),
                audio_secs: Some(4.94),
                video_start_secs: None,
                audio: vec![ClockAudioLeg {
                    index: 0,
                    duration_secs: Some(4.94),
                    start_secs: Some(0.021),
                }],
                correction_ms: -200,
            })
            .unwrap();
        store
            .append_clock(ClockSegment {
                id: SegmentId(1),
                start: t(0.0),
                end: t(5.0),
                master: ClockMaster::Video,
                session_secs: 5.1,
                video_secs: Some(5.0),
                audio_secs: None,
                video_start_secs: None,
                audio: Vec::new(),
                correction_ms: -100,
            })
            .unwrap();
        let back = store.read_clocks().unwrap().unwrap();
        assert_eq!(back.version, CLOCK_SIDECAR_VERSION);
        assert_eq!(back.segments.len(), 1);
        assert_eq!(back.segment(SegmentId(1)).unwrap().correction_ms, -100);
        let _ = std::fs::remove_dir_all(root);
    }
}
