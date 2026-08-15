//! Post-capture analysis of a committed session.
//!
//! Everything here runs **after** segments are closed, never on the live
//! grab: it re-reads finished files with host ffmpeg. Two jobs:
//!
//! * [`materialize_audio`] — demux each configured audio leg into its own
//!   single-stream file so a project clip can address it without guessing a
//!   stream index;
//! * [`session_signals`] — measure frame difference and audio energy so idle
//!   detection stops being pointer-only.
//!
//! Both are best-effort with respect to the host: a missing ffmpeg yields
//! fewer signals, never a wrong answer.

mod audio;
mod signals;

pub use audio::{AudioMaterialization, audio_rel_path, materialize_audio};
pub use signals::{SignalOptions, session_signals};
