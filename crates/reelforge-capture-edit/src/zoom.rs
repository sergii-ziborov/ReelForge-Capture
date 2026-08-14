//! Automatic zoom windows around clicks.

use reelforge_capture_core::{MediaRange, MediaTime, PointerEvent, Result};
use serde::{Deserialize, Serialize};

/// One click-zoom hint (crop/scale at compile).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClickZoom {
    /// Window on the capture clock.
    pub range: MediaRange,
    /// Click X (desktop pixels).
    pub x: i32,
    /// Click Y.
    pub y: i32,
    /// Scale factor (`1.8` = 180%).
    pub scale: f64,
}

/// Build zoom windows: `[t, t + duration)` for each click.
///
/// # Errors
///
/// Invalid duration.
pub fn zoom_from_clicks(
    events: &[PointerEvent],
    duration: MediaTime,
    scale: f64,
    session_end: MediaTime,
) -> Result<Vec<ClickZoom>> {
    let mut out = Vec::new();
    if duration.ticks <= 0 || !(scale.is_finite() && scale > 1.0) {
        return Ok(out);
    }
    let clock = session_end.timescale.max(1);
    for e in events {
        let PointerEvent::Click { t, x, y, .. } = e else {
            continue;
        };
        let end_ticks = (t.ticks + duration.ticks).min(session_end.ticks);
        if end_ticks <= t.ticks {
            continue;
        }
        let start = MediaTime::new(t.ticks, clock)?;
        let end = MediaTime::new(end_ticks, clock)?;
        out.push(ClickZoom {
            range: MediaRange::new(start, end)?,
            x: *x,
            y: *y,
            scale,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelforge_capture_core::{ClickButton, HZ_1K};

    #[test]
    fn one_click_one_window() {
        let ev = [PointerEvent::Click {
            t: MediaTime::from_secs(1.0, HZ_1K).unwrap(),
            x: 100,
            y: 80,
            button: ClickButton::Left,
        }];
        let z = zoom_from_clicks(
            &ev,
            MediaTime::from_secs(0.4, HZ_1K).unwrap(),
            1.8,
            MediaTime::from_secs(5.0, HZ_1K).unwrap(),
        )
        .unwrap();
        assert_eq!(z.len(), 1);
        assert_eq!(z[0].x, 100);
        assert!((z[0].range.duration_secs() - 0.4).abs() < 1e-9);
    }
}
