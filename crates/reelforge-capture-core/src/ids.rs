//! Session / segment identifiers.

use serde::{Deserialize, Serialize};

/// Capture session id (`ses_…`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    /// Construct.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// As str.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Closed media segment ordinal (`1`-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SegmentId(pub u32);

impl SegmentId {
    /// First segment.
    #[must_use]
    pub const fn first() -> Self {
        Self(1)
    }

    /// Next ordinal.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Zero-padded file stem.
    #[must_use]
    pub fn file_stem(self) -> String {
        format!("{:06}", self.0)
    }
}
