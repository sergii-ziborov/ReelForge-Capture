//! Trim / remove / speed on the session clock.

use reelforge_capture_core::{CaptureError, HZ_1K, MediaRange, MediaTime, Result};
use serde::{Deserialize, Serialize};

/// One editorial decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EditDecision {
    /// Keep only `[start, end)` of the source (session-level in/out).
    Trim {
        /// Inclusive start.
        start: MediaTime,
        /// Exclusive end.
        end: MediaTime,
    },
    /// Drop a range (ripple: later media shifts left on the record).
    Remove {
        /// Inclusive start.
        start: MediaTime,
        /// Exclusive end.
        end: MediaTime,
    },
    /// Constant speed on a source range (`2.0` = twice as fast).
    Speed {
        /// Inclusive start.
        start: MediaTime,
        /// Exclusive end.
        end: MediaTime,
        /// Factor.
        factor: f64,
    },
}

/// Ordered edit list (applied in order).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct EditList {
    /// Decisions.
    #[serde(default)]
    pub ops: Vec<EditDecision>,
}

/// One kept source interval after edits (record order).
#[derive(Debug, Clone, PartialEq)]
pub struct KeptRange {
    /// Source range on the capture clock.
    pub source: MediaRange,
    /// Playback factor (`1.0` = identity).
    pub speed: f64,
}

/// Apply trim / remove / speed. Speed ranges that overlap a keep are tagged.
///
/// # Errors
///
/// Empty / inverted ranges or non-positive speed.
pub fn apply_ranges(duration: MediaTime, list: &EditList) -> Result<Vec<KeptRange>> {
    if duration.ticks <= 0 {
        return Err(CaptureError::timing("session duration must be > 0"));
    }
    let scale = duration.timescale;
    let mut keep = vec![MediaRange::new(MediaTime::zero(scale), duration)?];
    let mut speeds: Vec<(MediaRange, f64)> = Vec::new();

    for op in &list.ops {
        match op {
            EditDecision::Trim { start, end } => {
                let window = MediaRange::new(*start, *end)?;
                keep = intersect_all(&keep, window);
            }
            EditDecision::Remove { start, end } => {
                let cut = MediaRange::new(*start, *end)?;
                keep = subtract_all(&keep, cut)?;
            }
            EditDecision::Speed { start, end, factor } => {
                if !(factor.is_finite() && *factor > 0.0) {
                    return Err(CaptureError::message(format!("bad speed {factor}")));
                }
                speeds.push((MediaRange::new(*start, *end)?, *factor));
            }
        }
    }

    let mut out = Vec::new();
    for k in keep {
        let factor = speeds
            .iter()
            .rev()
            .find(|(r, _)| ranges_overlap(*r, k))
            .map_or(1.0, |(_, f)| *f);
        out.push(KeptRange {
            source: k,
            speed: factor,
        });
    }
    let _ = HZ_1K;
    Ok(out)
}

fn ranges_overlap(a: MediaRange, b: MediaRange) -> bool {
    a.start.ticks < b.end.ticks && b.start.ticks < a.end.ticks
}

fn intersect_all(keep: &[MediaRange], window: MediaRange) -> Vec<MediaRange> {
    keep.iter().filter_map(|k| intersect(*k, window)).collect()
}

fn intersect(a: MediaRange, b: MediaRange) -> Option<MediaRange> {
    let start = a.start.ticks.max(b.start.ticks);
    let end = a.end.ticks.min(b.end.ticks);
    if end <= start {
        return None;
    }
    MediaRange::new(
        MediaTime {
            ticks: start,
            timescale: a.start.timescale,
        },
        MediaTime {
            ticks: end,
            timescale: a.start.timescale,
        },
    )
    .ok()
}

fn subtract_all(keep: &[MediaRange], cut: MediaRange) -> Result<Vec<MediaRange>> {
    let mut out = Vec::new();
    for k in keep {
        out.extend(subtract(*k, cut)?);
    }
    Ok(out)
}

fn subtract(keep: MediaRange, cut: MediaRange) -> Result<Vec<MediaRange>> {
    let Some(hit) = intersect(keep, cut) else {
        return Ok(vec![keep]);
    };
    let mut out = Vec::new();
    if hit.start.ticks > keep.start.ticks {
        out.push(MediaRange::new(keep.start, hit.start)?);
    }
    if hit.end.ticks < keep.end.ticks {
        out.push(MediaRange::new(hit.end, keep.end)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: f64) -> MediaTime {
        MediaTime::from_secs(s, HZ_1K).unwrap()
    }

    #[test]
    fn trim_then_remove() {
        let mut list = EditList::default();
        list.ops.push(EditDecision::Trim {
            start: t(1.0),
            end: t(10.0),
        });
        list.ops.push(EditDecision::Remove {
            start: t(3.0),
            end: t(4.0),
        });
        let kept = apply_ranges(t(20.0), &list).unwrap();
        assert_eq!(kept.len(), 2);
        assert!((kept[0].source.start.as_secs() - 1.0).abs() < 1e-9);
        assert!((kept[0].source.end.as_secs() - 3.0).abs() < 1e-9);
        assert!((kept[1].source.start.as_secs() - 4.0).abs() < 1e-9);
    }

    #[test]
    fn speed_tags_keep() {
        let mut list = EditList::default();
        list.ops.push(EditDecision::Speed {
            start: t(0.0),
            end: t(2.0),
            factor: 2.0,
        });
        let kept = apply_ranges(t(5.0), &list).unwrap();
        assert_eq!(kept.len(), 1);
        assert!((kept[0].speed - 2.0).abs() < 1e-9);
    }
}
