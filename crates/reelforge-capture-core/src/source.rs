//! Video and audio capture sources.

use serde::{Deserialize, Serialize};

/// Pixel rectangle on the virtual desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    /// Left.
    pub x: i32,
    /// Top.
    pub y: i32,
    /// Width (must be even for typical yuv420 encode).
    pub width: u32,
    /// Height (must be even).
    pub height: u32,
}

impl Region {
    /// Construct.
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// What to grab from the display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VideoSource {
    /// Full virtual desktop (`gdigrab` `desktop`).
    Screen,
    /// Window whose title contains `title`.
    Window {
        /// Substring match on the window title.
        title: String,
    },
    /// Crop of the desktop.
    Region {
        /// Pixel box.
        region: Region,
    },
}

/// Named WASAPI / `DirectShow` device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDevice {
    /// ffmpeg device name (as listed by `-list_devices`).
    pub name: String,
    /// `dshow` or `wasapi`.
    #[serde(default = "default_audio_backend")]
    pub backend: String,
}

fn default_audio_backend() -> String {
    "dshow".into()
}

impl AudioDevice {
    /// `dshow` device.
    #[must_use]
    pub fn dshow(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            backend: "dshow".into(),
        }
    }
}
