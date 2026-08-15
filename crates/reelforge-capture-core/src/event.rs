//! Session sidecar: pointer samples and capture-clock gaps (not a vision track).

use crate::time::MediaTime;
use serde::{Deserialize, Serialize};

/// Mouse button for a click.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClickButton {
    /// Left.
    Left,
    /// Right.
    Right,
    /// Middle.
    Middle,
}

/// Pointer sample or clock/health event on the capture clock.
///
/// JSON tag is `kind`. New variants are additive; readers must ignore unknown
/// kinds if they parse this log loosely.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PointerEvent {
    /// Cursor moved (or a periodic sample).
    Cursor {
        /// Session time.
        t: MediaTime,
        /// Desktop X.
        x: i32,
        /// Desktop Y.
        y: i32,
    },
    /// Button down.
    Click {
        /// Session time.
        t: MediaTime,
        /// Desktop X.
        x: i32,
        /// Desktop Y.
        y: i32,
        /// Which button.
        button: ClickButton,
    },
    /// Closed video segment duration drifted from the session clock.
    FrameGap {
        /// Session time of the commit.
        t: MediaTime,
        /// Expected seconds (segment budget or clock span).
        expected_secs: f64,
        /// `ffprobe` duration, if known.
        actual_secs: f64,
    },
    /// Closed audio duration drifted from the session clock.
    AudioGap {
        /// Session time of the commit.
        t: MediaTime,
        /// Expected seconds.
        expected_secs: f64,
        /// Probed audio stream duration.
        actual_secs: f64,
    },
    /// Session directory is out of space.
    DiskFull {
        /// Session time.
        t: MediaTime,
    },
    /// Grabber died or a device disappeared.
    DeviceLost {
        /// Session time.
        t: MediaTime,
        /// Host / ffmpeg detail.
        detail: String,
    },
}

impl PointerEvent {
    /// Event time.
    #[must_use]
    pub const fn time(&self) -> MediaTime {
        match *self {
            Self::Cursor { t, .. }
            | Self::Click { t, .. }
            | Self::FrameGap { t, .. }
            | Self::AudioGap { t, .. }
            | Self::DiskFull { t }
            | Self::DeviceLost { t, .. } => t,
        }
    }

    /// Cursor / click position.
    #[must_use]
    pub const fn position(&self) -> Option<(i32, i32)> {
        match *self {
            Self::Cursor { x, y, .. } | Self::Click { x, y, .. } => Some((x, y)),
            Self::FrameGap { .. }
            | Self::AudioGap { .. }
            | Self::DiskFull { .. }
            | Self::DeviceLost { .. } => None,
        }
    }

    /// Cursor or click (usable as idle / zoom evidence).
    #[must_use]
    pub const fn is_pointer(&self) -> bool {
        matches!(self, Self::Cursor { .. } | Self::Click { .. })
    }

    /// Whether this is a click.
    #[must_use]
    pub const fn is_click(&self) -> bool {
        matches!(self, Self::Click { .. })
    }
}
