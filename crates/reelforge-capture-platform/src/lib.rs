//! Host ffmpeg grab command + device listing (no libav).

mod audio;
mod ffmpeg;
mod host;
mod list;
mod video;

pub use ffmpeg::{FfmpegGrab, grab_command, grab_command_on, spawn_grab};
pub use host::HostOs;
pub use list::{
    AudioListing, WindowInfo, list_audio_hint, list_audio_hint_on, list_windows, list_windows_on,
    parse_tab_windows, parse_wmctrl,
};
