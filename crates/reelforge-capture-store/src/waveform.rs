//! `waveforms.json` — min/max peaks per audio leg on the session clock.
//!
//! The editor / project should read this instead of re-decoding audio.
//! Shape of each peak matches `ReelForge` `WaveformPeak` (`t0`/`t1`/`min`/`max`);
//! times are [`MediaTime`] so they sit on the same clock as the session.

use crate::SessionStore;
use reelforge_capture_core::{AudioLeg, MediaTime, Result};
use serde::{Deserialize, Serialize};
use std::fs;

/// Schema of [`WaveformSidecar`].
pub const WAVEFORM_SIDECAR_VERSION: u32 = 1;

/// One min/max bucket.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WaveformPeak {
    /// Window start on the session clock.
    pub t0: MediaTime,
    /// Window end on the session clock.
    pub t1: MediaTime,
    /// Minimum sample in the window (mono peak-hold).
    pub min: f32,
    /// Maximum sample in the window.
    pub max: f32,
}

/// Peaks for one configured audio leg.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaveformLeg {
    /// System or microphone.
    pub leg: AudioLeg,
    /// Envelope, in session time order.
    #[serde(default)]
    pub peaks: Vec<WaveformPeak>,
}

/// `waveforms.json` contents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaveformSidecar {
    /// [`WAVEFORM_SIDECAR_VERSION`].
    pub version: u32,
    /// PCM rate used to decode (not the original capture rate).
    pub sample_rate: u32,
    /// Bucket width in seconds.
    pub bucket_secs: f64,
    /// One entry per measured leg.
    #[serde(default)]
    pub legs: Vec<WaveformLeg>,
}

impl WaveformSidecar {
    /// Empty sidecar at the current version.
    #[must_use]
    pub const fn new(sample_rate: u32, bucket_secs: f64) -> Self {
        Self {
            version: WAVEFORM_SIDECAR_VERSION,
            sample_rate,
            bucket_secs,
            legs: Vec::new(),
        }
    }

    /// Peaks for one leg.
    #[must_use]
    pub fn leg(&self, leg: AudioLeg) -> Option<&WaveformLeg> {
        self.legs.iter().find(|l| l.leg == leg)
    }
}

impl SessionStore {
    /// Path of `waveforms.json`.
    #[must_use]
    pub fn waveform_path(&self) -> std::path::PathBuf {
        self.root.join("waveforms.json")
    }

    /// Read `waveforms.json` (`None` when never computed).
    ///
    /// # Errors
    ///
    /// I/O / JSON.
    pub fn read_waveforms(&self) -> Result<Option<WaveformSidecar>> {
        let path = self.waveform_path();
        if !path.is_file() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&fs::read_to_string(path)?)?))
    }

    /// Write `waveforms.json` atomically.
    ///
    /// # Errors
    ///
    /// I/O / JSON.
    pub fn write_waveforms(&self, sidecar: &WaveformSidecar) -> Result<()> {
        let dest = self.waveform_path();
        let tmp = self.root.join("waveforms.json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(sidecar)?)?;
        let _ = fs::remove_file(&dest);
        fs::rename(tmp, dest)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelforge_capture_core::{CaptureSpec, HZ_1K, SessionId, SessionMeta};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn t(secs: f64) -> MediaTime {
        MediaTime::from_secs(secs, HZ_1K).unwrap()
    }

    #[test]
    fn round_trips() {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rf-wf-{n}"));
        let store = SessionStore::create(
            &root,
            SessionMeta {
                id: SessionId::new("ses_w"),
                name: "w".into(),
                spec: CaptureSpec::screen(),
                started_unix: None,
                duration: None,
            },
        )
        .unwrap();
        let mut side = WaveformSidecar::new(8_000, 0.05);
        side.legs.push(WaveformLeg {
            leg: AudioLeg::Microphone,
            peaks: vec![WaveformPeak {
                t0: t(0.0),
                t1: t(0.05),
                min: -0.25,
                max: 0.4,
            }],
        });
        store.write_waveforms(&side).unwrap();
        let back = store.read_waveforms().unwrap().unwrap();
        assert_eq!(back, side);
        let _ = std::fs::remove_dir_all(root);
    }
}
