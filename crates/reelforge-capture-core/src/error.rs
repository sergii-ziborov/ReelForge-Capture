//! Capture errors.

/// Fallible capture / store / edit result.
pub type Result<T> = std::result::Result<T, CaptureError>;

/// Session, store, or platform failure.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CaptureError {
    /// Invalid timing.
    #[error("capture timing: {0}")]
    Timing(String),
    /// Bad argument or missing data.
    #[error("capture: {0}")]
    Message(String),
    /// Filesystem / I/O.
    #[error("capture io: {0}")]
    Io(String),
}

impl CaptureError {
    /// Structural / validation error.
    #[must_use]
    pub fn message(msg: impl Into<String>) -> Self {
        Self::Message(msg.into())
    }

    /// Timing error.
    #[must_use]
    pub fn timing(msg: impl Into<String>) -> Self {
        Self::Timing(msg.into())
    }

    /// I/O error.
    #[must_use]
    pub fn io(msg: impl Into<String>) -> Self {
        Self::Io(msg.into())
    }
}

impl From<std::io::Error> for CaptureError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<serde_json::Error> for CaptureError {
    fn from(e: serde_json::Error) -> Self {
        Self::Message(format!("json: {e}"))
    }
}
