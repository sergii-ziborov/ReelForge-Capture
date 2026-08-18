//! Live capture session supervisor.
//!
//! Store knows how to persist segments. Platform knows how to invoke ffmpeg.
//! Neither owns the *running* session: process lifetime, session clock,
//! prefix-commit of closed files, pause/resume, or crash vs clean-stop.
//! That owner is [`SessionSupervisor`].

mod clock;
mod grabber;
mod supervisor;

pub use clock::{
    AudioClockSample, ClockDecision, ClockSample, clock_row, decide_clocks, repair_clocks,
};
pub use grabber::{FakeGrabber, FfmpegGrabber, Grabber};
pub use supervisor::{LiveStatus, SessionPhase, SessionStatus, SessionSupervisor, SupervisorEvent};
