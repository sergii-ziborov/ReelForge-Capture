//! Host ffmpeg grab command + device listing (no libav).

mod audio;
mod disk;
mod ffmpeg;
mod host;
mod list;
mod pointer;
mod probe;
mod signals;
mod video;

pub use disk::available_bytes;
pub use ffmpeg::{FfmpegGrab, grab_command, grab_command_on, spawn_grab};
pub use host::HostOs;
pub use list::{
    AudioListing, WindowInfo, list_audio_hint, list_audio_hint_on, list_windows, list_windows_on,
    parse_tab_windows, parse_wmctrl,
};
pub use pointer::{FakePointer, HostPointer, NullPointer, PointerSample, PointerSource};
pub use probe::{parse_duration_secs, probe_audio_duration, probe_duration};
pub use signals::{
    AUDIO_RMS_KEY, MOTION_KEY, SILENCE_FLOOR_DB, audio_level_series, extract_audio_stream,
    motion_series, parse_metadata_series,
};
