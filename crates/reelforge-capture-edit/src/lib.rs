//! Non-destructive range edits, idle detection, and click zoom.

mod idle;
mod ranges;
mod zoom;

pub use idle::{IdleConfig, IdleReport, detect_idle, detect_idle_multi, quiet_ranges};
pub use ranges::{EditDecision, EditList, KeptRange, apply_ranges};
pub use zoom::{ClickZoom, zoom_from_clicks};
