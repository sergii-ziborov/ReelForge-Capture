//! Post-capture signal series (frame difference, audio energy).
//!
//! These are *measurements of recorded files*, not live telemetry: they are
//! produced after a segment is committed and consumed by idle detection. A
//! track with no samples means "not measured" — never "quiet".

use crate::time::{MediaRange, MediaTime};
use serde::{Deserialize, Serialize};

/// What a [`SignalTrack`] measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    /// Mean absolute frame difference (0 = identical frames).
    Motion,
    /// Audio energy in dBFS (`-120` = silence floor).
    AudioLevel,
}

impl SignalKind {
    /// Stable label for logs / reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Motion => "motion",
            Self::AudioLevel => "audio_level",
        }
    }
}

/// One measurement on the session clock.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SignalSample {
    /// Session time of the window this sample describes.
    pub t: MediaTime,
    /// Measured value (units depend on [`SignalKind`]).
    pub value: f64,
}

/// A sampled signal over the session clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalTrack {
    /// What is measured.
    pub kind: SignalKind,
    /// Where it came from (`video`, `audio:system`, `audio:microphone`).
    pub source: String,
    /// Nominal spacing between samples.
    pub period: MediaTime,
    /// Samples in time order.
    #[serde(default)]
    pub samples: Vec<SignalSample>,
}

impl SignalTrack {
    /// Empty track with a sampling period.
    #[must_use]
    pub fn new(kind: SignalKind, source: impl Into<String>, period: MediaTime) -> Self {
        Self {
            kind,
            source: source.into(),
            period,
            samples: Vec::new(),
        }
    }

    /// `kind:source`, e.g. `audio_level:audio:microphone`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.source)
    }

    /// Whether the track carries enough samples to state anything.
    #[must_use]
    pub fn is_measured(&self) -> bool {
        self.samples.len() >= 2 && self.period.ticks > 0
    }

    /// Session span the samples actually cover (`None` when unmeasured).
    #[must_use]
    pub fn coverage(&self) -> Option<MediaRange> {
        if !self.is_measured() {
            return None;
        }
        let first = self.samples.first()?.t;
        let last = self.samples.last()?.t;
        let scale = self.period.timescale.max(1);
        MediaRange::new(
            MediaTime {
                ticks: first.ticks,
                timescale: scale,
            },
            MediaTime {
                ticks: last.ticks.saturating_add(self.period.ticks),
                timescale: scale,
            },
        )
        .ok()
    }

    /// Append a sample, keeping time order.
    pub fn push(&mut self, t: MediaTime, value: f64) {
        self.samples.push(SignalSample { t, value });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::HZ_1K;

    fn t(secs: f64) -> MediaTime {
        MediaTime::from_secs(secs, HZ_1K).unwrap()
    }

    #[test]
    fn one_sample_is_not_measured() {
        let mut track = SignalTrack::new(SignalKind::Motion, "video", t(0.5));
        assert!(!track.is_measured());
        track.push(t(0.0), 0.0);
        assert!(!track.is_measured());
        assert!(track.coverage().is_none());
        track.push(t(0.5), 0.0);
        assert!(track.is_measured());
    }

    #[test]
    fn coverage_extends_by_one_period() {
        let mut track = SignalTrack::new(SignalKind::AudioLevel, "audio:system", t(0.5));
        track.push(t(1.0), -60.0);
        track.push(t(1.5), -60.0);
        let c = track.coverage().unwrap();
        assert!((c.start.as_secs() - 1.0).abs() < 1e-9);
        assert!((c.end.as_secs() - 2.0).abs() < 1e-9);
        assert_eq!(track.label(), "audio_level:audio:system");
    }
}
