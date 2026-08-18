//! Pure helpers shared by the host collectors (testable without X11 / Quartz).

/// X11 `XQueryPointer` button mask bits (`X.h`).
pub const BUTTON1_MASK: u32 = 1 << 8;
pub const BUTTON2_MASK: u32 = 1 << 9;
pub const BUTTON3_MASK: u32 = 1 << 10;

/// Left / middle / right from an X11 modifier+button mask.
#[must_use]
pub const fn buttons_from_mask(mask: u32) -> (bool, bool, bool) {
    (
        mask & BUTTON1_MASK != 0,
        mask & BUTTON3_MASK != 0,
        mask & BUTTON2_MASK != 0,
    )
}

/// Whether `XQueryKeymap`'s 32-byte map has any key down.
#[must_use]
pub fn keymap_any_down(keys: &[u8; 32]) -> bool {
    keys.iter().any(|&b| b != 0)
}

/// First on-screen, layer-0 window (Quartz list is front-to-back).
#[must_use]
pub fn front_window(windows: &[(i64, i32, String)]) -> Option<(u64, String)> {
    windows
        .iter()
        .find(|(_, layer, _)| *layer == 0)
        .map(|(id, _, title)| ((*id).cast_unsigned(), title.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x11_mask_maps_left_right_middle() {
        assert_eq!(buttons_from_mask(0), (false, false, false));
        assert_eq!(buttons_from_mask(BUTTON1_MASK), (true, false, false));
        assert_eq!(buttons_from_mask(BUTTON3_MASK), (false, true, false));
        assert_eq!(buttons_from_mask(BUTTON2_MASK), (false, false, true));
        assert_eq!(
            buttons_from_mask(BUTTON1_MASK | BUTTON3_MASK),
            (true, true, false)
        );
    }

    #[test]
    fn keymap_detects_any_bit() {
        let mut keys = [0u8; 32];
        assert!(!keymap_any_down(&keys));
        keys[4] = 0x10;
        assert!(keymap_any_down(&keys));
    }

    #[test]
    fn quartz_skips_menu_layer() {
        let list = [
            (7_i64, 25, "Menu Bar".into()),
            (42_i64, 0, "Safari".into()),
            (9_i64, 0, "Behind".into()),
        ];
        assert_eq!(front_window(&list), Some((42, "Safari".into())));
    }
}
