//! Session spec (what to record) and lightweight metadata.

use crate::ids::SessionId;
use crate::source::{AudioDevice, VideoSource};
use crate::time::MediaTime;
use serde::{Deserialize, Serialize};

/// One configured audio input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioLeg {
    /// Loopback / monitor (system output).
    System,
    /// Microphone.
    Microphone,
}

impl AudioLeg {
    /// Encode order: system first, then microphone.
    pub const ORDER: [Self; 2] = [Self::System, Self::Microphone];

    /// Stable label used in project tags and sidecar files.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Microphone => "microphone",
        }
    }
}

/// Which audio legs to open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AudioMix {
    /// Loopback / monitor (system output).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<AudioDevice>,
    /// Microphone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphone: Option<AudioDevice>,
}

/// How a session is captured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureSpec {
    /// Video source.
    pub video: VideoSource,
    /// Audio legs.
    #[serde(default)]
    pub audio: AudioMix,
    /// Frames per second for the host grabber.
    #[serde(default = "default_fps")]
    pub fps: f64,
    /// Closed-segment length in seconds.
    #[serde(default = "default_segment_secs")]
    pub segment_secs: f64,
    /// Also sample the cursor.
    #[serde(default = "default_true")]
    pub pointer: bool,
}

fn default_fps() -> f64 {
    30.0
}
fn default_segment_secs() -> f64 {
    5.0
}
fn default_true() -> bool {
    true
}

impl AudioMix {
    /// Configured legs in encode order: system, then microphone.
    #[must_use]
    pub fn legs(&self) -> Vec<&AudioDevice> {
        [&self.system, &self.microphone]
            .into_iter()
            .flatten()
            .collect()
    }

    /// Device for one leg.
    #[must_use]
    pub const fn device(&self, leg: AudioLeg) -> Option<&AudioDevice> {
        match leg {
            AudioLeg::System => self.system.as_ref(),
            AudioLeg::Microphone => self.microphone.as_ref(),
        }
    }

    /// Configured legs with their kind, in encode order.
    #[must_use]
    pub fn configured(&self) -> Vec<(AudioLeg, &AudioDevice)> {
        AudioLeg::ORDER
            .into_iter()
            .filter_map(|leg| self.device(leg).map(|dev| (leg, dev)))
            .collect()
    }

    /// Index of a leg **among the audio streams** of the mux (`ffmpeg -map 0:a:N`).
    ///
    /// Depends on what is actually configured: with no system leg the
    /// microphone is `a:0`, not `a:1`.
    #[must_use]
    pub fn audio_index(&self, leg: AudioLeg) -> Option<u32> {
        let mut idx = 0;
        for candidate in AudioLeg::ORDER {
            if self.device(candidate).is_none() {
                continue;
            }
            if candidate == leg {
                return Some(idx);
            }
            idx += 1;
        }
        None
    }

    /// Index of a leg among **all** streams of a Capture segment (video is `0`).
    #[must_use]
    pub fn stream_index(&self, leg: AudioLeg) -> Option<u32> {
        self.audio_index(leg).map(|i| i + 1)
    }

    /// At least one audio input.
    #[must_use]
    pub const fn has_any(&self) -> bool {
        self.system.is_some() || self.microphone.is_some()
    }
}

impl CaptureSpec {
    /// Desktop, no audio.
    #[must_use]
    pub fn screen() -> Self {
        Self {
            video: VideoSource::Screen,
            audio: AudioMix::default(),
            fps: default_fps(),
            segment_secs: default_segment_secs(),
            pointer: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_only_is_the_first_audio_stream() {
        let mix = AudioMix {
            system: None,
            microphone: Some(AudioDevice::named("mic")),
        };
        assert_eq!(mix.audio_index(AudioLeg::Microphone), Some(0));
        assert_eq!(mix.stream_index(AudioLeg::Microphone), Some(1));
        assert_eq!(mix.audio_index(AudioLeg::System), None);
        assert_eq!(mix.configured().len(), 1);
    }

    #[test]
    fn system_then_mic_keeps_encode_order() {
        let mix = AudioMix {
            system: Some(AudioDevice::named("loop")),
            microphone: Some(AudioDevice::named("mic")),
        };
        assert_eq!(mix.audio_index(AudioLeg::System), Some(0));
        assert_eq!(mix.audio_index(AudioLeg::Microphone), Some(1));
        assert_eq!(mix.stream_index(AudioLeg::Microphone), Some(2));
        let legs: Vec<_> = mix.configured().iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(legs, ["system", "microphone"]);
    }

    #[test]
    fn no_audio_has_no_indices() {
        let mix = AudioMix::default();
        assert!(!mix.has_any());
        assert_eq!(mix.stream_index(AudioLeg::System), None);
        assert!(mix.configured().is_empty());
    }
}

/// Recoverable session header (not the full store).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Id.
    pub id: SessionId,
    /// Display name.
    pub name: String,
    /// Spec used to start.
    pub spec: CaptureSpec,
    /// Wall-clock start (unix seconds), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_unix: Option<i64>,
    /// Closed duration (excludes an unfinished tail).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<MediaTime>,
}
