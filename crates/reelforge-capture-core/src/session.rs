//! Session spec (what to record) and lightweight metadata.

use crate::ids::SessionId;
use crate::source::{AudioDevice, VideoSource};
use crate::time::MediaTime;
use serde::{Deserialize, Serialize};

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
