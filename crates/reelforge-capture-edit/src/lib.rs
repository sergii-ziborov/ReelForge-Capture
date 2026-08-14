//! Non-destructive range edits, idle detection, and click zoom.

mod idle;
mod ranges;
mod zoom;

pub use idle::detect_idle;
pub use ranges::{EditDecision, EditList, KeptRange, apply_ranges};
pub use zoom::{ClickZoom, zoom_from_clicks};
