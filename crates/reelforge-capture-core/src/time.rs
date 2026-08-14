//! `ticks / timescale` — same shape as `ReelForge` / Intelligence `MediaTime`.

use crate::error::{CaptureError, Result};
use serde::{Deserialize, Serialize};

/// Default 1 kHz clock for editorial seconds.
pub const HZ_1K: u32 = 1_000;

/// Rational media time (`ticks / timescale` seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MediaTime {
    /// Tick count.
    pub ticks: i64,
    /// Ticks per second.
    pub timescale: u32,
}

impl MediaTime {
    /// Origin.
    #[must_use]
    pub const fn zero(timescale: u32) -> Self {
        Self {
            ticks: 0,
            timescale: if timescale == 0 { 1 } else { timescale },
        }
    }

    /// Construct.
    ///
    /// # Errors
    ///
    /// Zero timescale.
    pub fn new(ticks: i64, timescale: u32) -> Result<Self> {
        if timescale == 0 {
            return Err(CaptureError::timing("timescale must be > 0"));
        }
        Ok(Self { ticks, timescale })
    }

    /// From seconds at `timescale`.
    ///
    /// # Errors
    ///
    /// Zero timescale or non-finite seconds.
    #[allow(clippy::cast_possible_truncation)]
    pub fn from_secs(secs: f64, timescale: u32) -> Result<Self> {
        if timescale == 0 {
            return Err(CaptureError::timing("timescale must be > 0"));
        }
        if !secs.is_finite() {
            return Err(CaptureError::timing(format!("non-finite seconds {secs}")));
        }
        Ok(Self {
            ticks: (secs * f64::from(timescale)).round() as i64,
            timescale,
        })
    }

    /// Seconds as `f64`.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn as_secs(self) -> f64 {
        self.ticks as f64 / f64::from(self.timescale.max(1))
    }
}

/// Half-open range `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRange {
    /// Inclusive start.
    pub start: MediaTime,
    /// Exclusive end.
    pub end: MediaTime,
}

impl MediaRange {
    /// Construct when `end` is after `start` on the same clock.
    ///
    /// # Errors
    ///
    /// Mismatched timescale or empty / inverted range.
    pub fn new(start: MediaTime, end: MediaTime) -> Result<Self> {
        if start.timescale != end.timescale {
            return Err(CaptureError::timing("range timescale mismatch"));
        }
        if end.ticks <= start.ticks {
            return Err(CaptureError::timing("range must be non-empty"));
        }
        Ok(Self { start, end })
    }

    /// Length in seconds.
    #[must_use]
    pub fn duration_secs(self) -> f64 {
        (self.end.as_secs() - self.start.as_secs()).max(0.0)
    }

    /// Whether `t` is inside `[start, end)`.
    #[must_use]
    pub fn contains(self, t: MediaTime) -> bool {
        t.timescale == self.start.timescale
            && t.ticks >= self.start.ticks
            && t.ticks < self.end.ticks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secs_roundtrip() {
        let t = MediaTime::from_secs(1.5, HZ_1K).unwrap();
        assert_eq!(t.ticks, 1500);
        assert!((t.as_secs() - 1.5).abs() < 1e-9);
    }

    #[test]
    fn range_contains() {
        let r = MediaRange::new(
            MediaTime::from_secs(1.0, HZ_1K).unwrap(),
            MediaTime::from_secs(2.0, HZ_1K).unwrap(),
        )
        .unwrap();
        assert!(r.contains(MediaTime::from_secs(1.0, HZ_1K).unwrap()));
        assert!(!r.contains(MediaTime::from_secs(2.0, HZ_1K).unwrap()));
    }
}
