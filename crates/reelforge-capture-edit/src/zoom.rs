//! Automatic zoom windows around clicks.

use reelforge_capture_core::{MediaRange, MediaTime, PointerEvent, Result};
use serde::{Deserialize, Serialize};

/// One click-zoom hint (crop/scale at compile).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClickZoom {
    /// Window on the capture clock.
    pub range: MediaRange,
    /// Click X (desktop / frame pixels).
    pub x: i32,
    /// Click Y.
    pub y: i32,
    /// Scale factor (`1.8` = 180%).
    pub scale: f64,
}

/// Pixel crop inside the source frame (even width/height for yuv420).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CropRect {
    /// Left.
    pub x: u32,
    /// Top.
    pub y: u32,
    /// Width.
    pub w: u32,
    /// Height.
    pub h: u32,
}

/// One renderable zoom slice (ease-in / hold / ease-out).
#[derive(Debug, Clone, PartialEq)]
pub struct ZoomSlice {
    /// Session range this crop applies to.
    pub range: MediaRange,
    /// Crop in source pixels.
    pub crop: CropRect,
    /// Scale the crop back to this size (usually the full frame).
    pub scale_to: (u32, u32),
    /// Scale that produced the crop (`1.0` = no zoom).
    pub scale: f64,
}

impl ZoomSlice {
    /// Whether this slice actually changes the picture.
    #[must_use]
    pub fn is_zoomed(&self) -> bool {
        self.scale > 1.0 + 1e-6 && (self.crop.w < self.scale_to.0 || self.crop.h < self.scale_to.1)
    }
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

/// Safe crop centred on `(x, y)` at `scale`, clamped to the frame.
///
/// Width/height are even. A scale `≤ 1` or a degenerate frame yields the
/// full frame (no zoom).
#[must_use]
pub fn crop_around(x: i32, y: i32, scale: f64, frame_w: u32, frame_h: u32) -> CropRect {
    if frame_w < 2 || frame_h < 2 || !scale.is_finite() || scale <= 1.0 {
        return CropRect {
            x: 0,
            y: 0,
            w: frame_w,
            h: frame_h,
        };
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let w = even(((f64::from(frame_w) / scale).round() as u32).clamp(2, frame_w));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let h = even(((f64::from(frame_h) / scale).round() as u32).clamp(2, frame_h));
    let cx = u32::try_from(x.max(0))
        .unwrap_or(0)
        .min(frame_w.saturating_sub(1));
    let cy = u32::try_from(y.max(0))
        .unwrap_or(0)
        .min(frame_h.saturating_sub(1));
    let max_x = frame_w.saturating_sub(w);
    let max_y = frame_h.saturating_sub(h);
    CropRect {
        x: cx.saturating_sub(w / 2).min(max_x),
        y: cy.saturating_sub(h / 2).min(max_y),
        w,
        h,
    }
}

const fn even(n: u32) -> u32 {
    n & !1
}

/// Turn each click-zoom into ease-in / hold / ease-out slices.
///
/// Windows shorter than 300 ms stay a single hold. Longer ones spend 20 %
/// ramping in, 60 % held, 20 % ramping out. The mid-ease scale is halfway
/// between 1× and the target.
///
/// # Errors
///
/// Invalid range construction.
pub fn zoom_slices(zooms: &[ClickZoom], frame_w: u32, frame_h: u32) -> Result<Vec<ZoomSlice>> {
    let mut out = Vec::new();
    for z in zooms {
        if !(z.scale.is_finite() && z.scale > 1.0) || frame_w < 2 || frame_h < 2 {
            continue;
        }
        let span = z.range.end.ticks - z.range.start.ticks;
        let scale = z.range.start.timescale;
        let parts: &[(f64, f64)] = if span < 300 {
            &[(0.0, 1.0)]
        } else {
            &[(0.0, 0.2), (0.2, 0.8), (0.8, 1.0)]
        };
        for (i, (a, b)) in parts.iter().copied().enumerate() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
            let start = z.range.start.ticks + ((span as f64) * a).round() as i64;
            #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
            let end = z.range.start.ticks + ((span as f64) * b).round() as i64;
            if end <= start {
                continue;
            }
            let hold = parts.len() == 1 || i == 1;
            let factor = if hold {
                z.scale
            } else {
                1.0 + (z.scale - 1.0) * 0.5
            };
            let crop = crop_around(z.x, z.y, factor, frame_w, frame_h);
            let slice = ZoomSlice {
                range: MediaRange::new(
                    MediaTime {
                        ticks: start,
                        timescale: scale,
                    },
                    MediaTime {
                        ticks: end,
                        timescale: scale,
                    },
                )?,
                crop,
                scale_to: (frame_w, frame_h),
                scale: factor,
            };
            if slice.is_zoomed() {
                out.push(slice);
            }
        }
    }
    Ok(out)
}

/// Overlap of `[start, end)` with `zoom` on the same clock, if any.
#[must_use]
pub fn zoom_overlap(start: i64, end: i64, zoom: &ZoomSlice) -> Option<(i64, i64)> {
    let a = start.max(zoom.range.start.ticks);
    let b = end.min(zoom.range.end.ticks);
    (b > a).then_some((a, b))
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

    #[test]
    fn crop_stays_inside_the_frame() {
        let c = crop_around(10, 10, 2.0, 320, 180);
        assert_eq!(c.w % 2, 0);
        assert_eq!(c.h % 2, 0);
        assert!(c.x + c.w <= 320);
        assert!(c.y + c.h <= 180);
        assert_eq!(c.w, 160);
        assert_eq!(c.h, 90);
        // Near the corner: origin clamps to 0, not a negative crop.
        let corner = crop_around(0, 0, 2.0, 320, 180);
        assert_eq!(corner.x, 0);
        assert_eq!(corner.y, 0);
    }

    #[test]
    fn long_zoom_splits_into_ease_hold_ease() {
        let z = ClickZoom {
            range: MediaRange::new(
                MediaTime::from_secs(1.0, HZ_1K).unwrap(),
                MediaTime::from_secs(2.0, HZ_1K).unwrap(),
            )
            .unwrap(),
            x: 160,
            y: 90,
            scale: 2.0,
        };
        let slices = zoom_slices(&[z], 320, 180).unwrap();
        assert_eq!(slices.len(), 3);
        assert!((slices[1].scale - 2.0).abs() < 1e-9);
        assert!(slices[0].scale < slices[1].scale);
        assert!((slices[0].scale - slices[2].scale).abs() < 1e-9);
        assert_eq!(slices[1].crop.w, 160);
    }
}
