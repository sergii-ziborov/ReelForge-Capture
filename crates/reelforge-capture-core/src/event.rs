//! Cursor and click metadata (not a vision track).

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

/// Pointer sample on the capture clock.
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
}

impl PointerEvent {
    /// Event time.
    #[must_use]
    pub const fn time(&self) -> MediaTime {
        match *self {
            Self::Cursor { t, .. } | Self::Click { t, .. } => t,
        }
    }

    /// Position.
    #[must_use]
    pub const fn position(&self) -> (i32, i32) {
        match *self {
            Self::Cursor { x, y, .. } | Self::Click { x, y, .. } => (x, y),
        }
    }

    /// Whether this is a click.
    #[must_use]
    pub const fn is_click(&self) -> bool {
        matches!(self, Self::Click { .. })
    }
}
