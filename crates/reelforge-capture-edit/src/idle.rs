//! Idle ranges from a pointer log (no move and no click).

use reelforge_capture_core::{MediaRange, MediaTime, PointerEvent, Result};

/// Detect stretches where the cursor is still and nobody clicks.
///
/// `threshold` is the minimum still time. The last sample is held until
/// `duration` (session end).
///
/// # Errors
///
/// Invalid range construction.
pub fn detect_idle(
    events: &[PointerEvent],
    duration: MediaTime,
    threshold: MediaTime,
) -> Result<Vec<MediaRange>> {
    let scale = duration.timescale.max(1);
    let need = threshold.as_secs();
    if need <= 0.0 || duration.ticks <= 0 {
        return Ok(Vec::new());
    }

    let mut marks: Vec<(f64, i32, i32, bool)> = Vec::new();
    for e in events {
        let (x, y) = e.position();
        marks.push((e.time().as_secs(), x, y, e.is_click()));
    }
    marks.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let end_s = duration.as_secs();
    if marks.is_empty() {
        return if end_s >= need {
            Ok(vec![MediaRange::new(MediaTime::zero(scale), duration)?])
        } else {
            Ok(Vec::new())
        };
    }

    let mut idle = Vec::new();
    // Before first event: idle if long enough.
    if marks[0].0 >= need {
        idle.push(MediaRange::new(
            MediaTime::zero(scale),
            MediaTime::from_secs(marks[0].0, scale)?,
        )?);
    }
    let mut last_t = marks[0].0;
    let mut last_xy = marks[0];

    for m in marks
        .iter()
        .skip(1)
        .copied()
        .chain(std::iter::once((end_s, last_xy.1, last_xy.2, false)))
    {
        let dt = m.0 - last_t;
        let moved = m.1 != last_xy.1 || m.2 != last_xy.2;
        let click = last_xy.3 || m.3;
        if dt + 1e-9 >= need && !moved && !click {
            idle.push(MediaRange::new(
                MediaTime::from_secs(last_t, scale)?,
                MediaTime::from_secs(m.0, scale)?,
            )?);
        }
        if m.0 < end_s || marks.len() == 1 {
            last_t = m.0;
            last_xy = m;
        } else {
            last_t = m.0;
        }
    }
    Ok(idle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelforge_capture_core::{HZ_1K, PointerEvent};

    fn cur(s: f64, x: i32, y: i32) -> PointerEvent {
        PointerEvent::Cursor {
            t: MediaTime::from_secs(s, HZ_1K).unwrap(),
            x,
            y,
        }
    }

    #[test]
    fn still_cursor_is_idle() {
        let ev = vec![cur(0.0, 1, 1), cur(4.0, 1, 1)];
        let idle = detect_idle(
            &ev,
            MediaTime::from_secs(4.0, HZ_1K).unwrap(),
            MediaTime::from_secs(2.0, HZ_1K).unwrap(),
        )
        .unwrap();
        assert_eq!(idle.len(), 1);
        assert!((idle[0].duration_secs() - 4.0).abs() < 1e-9);
    }

    #[test]
    fn motion_breaks_idle() {
        let ev = vec![cur(0.0, 0, 0), cur(1.0, 50, 0), cur(2.0, 50, 0)];
        let idle = detect_idle(
            &ev,
            MediaTime::from_secs(2.0, HZ_1K).unwrap(),
            MediaTime::from_secs(1.5, HZ_1K).unwrap(),
        )
        .unwrap();
        assert!(idle.is_empty());
    }
}
