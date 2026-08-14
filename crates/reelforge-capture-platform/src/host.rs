//! Host OS for ffmpeg grab / device listing.

/// Machine we are building a grab for (overridable in tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    /// `gdigrab` + `dshow` / `wasapi`.
    Windows,
    /// `avfoundation` (screen index + audio device).
    Macos,
    /// `x11grab` + `pulse` (`PipeWire` via Pulse compat).
    Linux,
}

impl HostOs {
    /// Compile-target default.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else {
            Self::Linux
        }
    }

    /// ffmpeg audio `-f` when the device does not set a backend.
    #[must_use]
    pub const fn default_audio_backend(self) -> &'static str {
        match self {
            Self::Windows => "dshow",
            Self::Macos => "avfoundation",
            Self::Linux => "pulse",
        }
    }

    /// ffmpeg video input format.
    #[must_use]
    pub const fn video_format(self) -> &'static str {
        match self {
            Self::Windows => "gdigrab",
            Self::Macos => "avfoundation",
            Self::Linux => "x11grab",
        }
    }
}
