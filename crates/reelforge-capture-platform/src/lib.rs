//! Host ffmpeg grab command + device listing (no libav).

mod ffmpeg;
mod list;

pub use ffmpeg::{FfmpegGrab, grab_command, spawn_grab};
pub use list::{AudioListing, WindowInfo, list_audio_hint, list_windows};
