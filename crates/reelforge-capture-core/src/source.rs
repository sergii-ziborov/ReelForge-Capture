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
    /// Full desktop / session display (`gdigrab` / `avfoundation` / `x11grab`).
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

/// Named host audio device (backend filled at grab if empty).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDevice {
    /// ffmpeg device name or index (as listed by the host backend).
    pub name: String,
    /// `dshow` / `wasapi` / `avfoundation` / `pulse` / `alsa`. Empty → host default.
    #[serde(default)]
    pub backend: String,
}

impl AudioDevice {
    /// Device name; backend chosen for the host OS at grab time.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            backend: String::new(),
        }
    }

    /// `dshow` device (Windows).
    #[must_use]
    pub fn dshow(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            backend: "dshow".into(),
        }
    }

    /// `avfoundation` device (macOS).
    #[must_use]
    pub fn avfoundation(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            backend: "avfoundation".into(),
        }
    }

    /// Pulse / PipeWire-Pulse source (Linux).
    #[must_use]
    pub fn pulse(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            backend: "pulse".into(),
        }
    }
}
