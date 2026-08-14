//! Shared types for `ReelForge` Capture (not a render engine, not vision).

mod error;
mod event;
mod ids;
mod session;
mod source;
mod time;

pub use error::{CaptureError, Result};
pub use event::{ClickButton, PointerEvent};
pub use ids::{SegmentId, SessionId};
pub use session::{AudioMix, CaptureSpec, SessionMeta};
pub use source::{AudioDevice, Region, VideoSource};
pub use time::{HZ_1K, MediaRange, MediaTime};
