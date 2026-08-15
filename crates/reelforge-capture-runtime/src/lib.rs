//! Live capture session supervisor.
//!
//! Store knows how to persist segments. Platform knows how to invoke ffmpeg.
//! Neither owns the *running* session: process lifetime, session clock,
//! prefix-commit of closed files, pause/resume, or crash vs clean-stop.
//! That owner is [`SessionSupervisor`].

mod grabber;
mod supervisor;

pub use grabber::{FakeGrabber, FfmpegGrabber, Grabber};
pub use supervisor::{SessionPhase, SessionStatus, SessionSupervisor, SupervisorEvent};
