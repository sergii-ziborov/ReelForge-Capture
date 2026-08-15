//! `audio.json` — where each audio leg physically lives after capture.
//!
//! A Capture segment muxes every configured leg into one `.mkv`. A project
//! clip cannot point at "stream 2 of this file", so each leg is demuxed into
//! its own single-stream file and recorded here. Drift between the demuxed
//! audio and its video segment is kept as a first-class `gap` instead of
//! being rounded away.

use reelforge_capture_core::{AudioLeg, MediaTime, SegmentId};
use serde::{Deserialize, Serialize};

/// Schema of [`AudioSidecar`].
pub const AUDIO_SIDECAR_VERSION: u32 = 1;

/// `audio.json` contents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioSidecar {
    /// [`AUDIO_SIDECAR_VERSION`].
    pub version: u32,
    /// One entry per configured leg, in encode order.
    #[serde(default)]
    pub legs: Vec<AudioLegTrack>,
}

impl AudioSidecar {
    /// Empty sidecar at the current version.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            version: AUDIO_SIDECAR_VERSION,
            legs: Vec::new(),
        }
    }

    /// Entry for one leg.
    #[must_use]
    pub fn leg(&self, leg: AudioLeg) -> Option<&AudioLegTrack> {
        self.legs.iter().find(|l| l.leg == leg)
    }

    /// Whether any leg produced a file.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.legs.iter().all(|l| l.files.is_empty())
    }
}

impl Default for AudioSidecar {
    fn default() -> Self {
        Self::new()
    }
}

/// One leg's demuxed files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioLegTrack {
    /// System or microphone.
    pub leg: AudioLeg,
    /// Host device the leg was opened with.
    #[serde(default)]
    pub device: String,
    /// `a:N` index this leg had inside the segment mux.
    pub audio_index: u32,
    /// Demuxed files, one per committed segment, in order.
    #[serde(default)]
    pub files: Vec<AudioSegmentFile>,
}

/// One demuxed audio file covering one video segment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioSegmentFile {
    /// Segment this file was demuxed from.
    pub segment: SegmentId,
    /// Path relative to the session root.
    pub path: String,
    /// Start on the session clock (from the segment record).
    pub start: MediaTime,
    /// End on the session clock (from the segment record).
    pub end: MediaTime,
    /// Probed duration of the demuxed file, when ffprobe could read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<MediaTime>,
    /// `duration − (end − start)`: audio short of (negative) or past (positive)
    /// its video segment. Kept, not corrected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<MediaTime>,
}

impl AudioSegmentFile {
    /// Session span of the video segment this file belongs to.
    #[must_use]
    pub fn span_ticks(&self) -> i64 {
        (self.end.ticks - self.start.ticks).max(0)
    }

    /// Drift in milliseconds, when probed.
    #[must_use]
    pub fn gap_ms(&self) -> Option<i64> {
        self.gap.map(|g| {
            let scale = i64::from(g.timescale.max(1));
            g.ticks * 1_000 / scale
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelforge_capture_core::HZ_1K;

    fn t(secs: f64) -> MediaTime {
        MediaTime::from_secs(secs, HZ_1K).unwrap()
    }

    fn sidecar() -> AudioSidecar {
        AudioSidecar {
            version: AUDIO_SIDECAR_VERSION,
            legs: vec![AudioLegTrack {
                leg: AudioLeg::Microphone,
                device: "mic".into(),
                audio_index: 0,
                files: vec![AudioSegmentFile {
                    segment: SegmentId(1),
                    path: "audio/microphone/000001.m4a".into(),
                    start: t(0.0),
                    end: t(5.0),
                    duration: Some(t(4.94)),
                    gap: Some(MediaTime {
                        ticks: -60,
                        timescale: HZ_1K,
                    }),
                }],
            }],
        }
    }

    #[test]
    fn round_trips_and_keeps_negative_drift() {
        let s = sidecar();
        let text = serde_json::to_string_pretty(&s).unwrap();
        assert!(text.contains("\"leg\": \"microphone\""), "{text}");
        let back: AudioSidecar = serde_json::from_str(&text).unwrap();
        assert_eq!(back, s);
        let file = &back.leg(AudioLeg::Microphone).unwrap().files[0];
        assert_eq!(file.gap_ms(), Some(-60));
        assert_eq!(file.span_ticks(), 5_000);
    }

    #[test]
    fn empty_sidecar_reports_no_files() {
        assert!(AudioSidecar::new().is_empty());
        assert!(!sidecar().is_empty());
    }
}
