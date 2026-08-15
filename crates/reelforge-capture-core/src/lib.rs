//! Shared types for `ReelForge` Capture (not a render engine, not vision).

mod error;
mod event;
mod ids;
mod session;
mod signal;
mod source;
mod time;

pub use error::{CaptureError, Result};
pub use event::{ClickButton, PointerEvent};
pub use ids::{SegmentId, SessionId};
pub use session::{AudioLeg, AudioMix, CaptureSpec, SessionMeta};
pub use signal::{SignalKind, SignalSample, SignalTrack};
pub use source::{AudioDevice, Region, VideoSource};
pub use time::{HZ_1K, MediaRange, MediaTime};
