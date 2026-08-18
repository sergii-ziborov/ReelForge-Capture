//! Windows cursor / buttons / keys / foreground window.

use super::PointerSample;

#[derive(Debug, Default)]
pub struct Collector;

impl Collector {
    #[allow(clippy::unused_self)]
    pub fn sample(&mut self) -> Option<PointerSample> {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
            GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

        let mut pt = POINT { x: 0, y: 0 };
        let ok = unsafe { GetCursorPos(&raw mut pt) };
        if ok == 0 {
            return None;
        }
        let down = |vk: u16| {
            let s = unsafe { GetAsyncKeyState(i32::from(vk)) };
            s < 0
        };
        let (window, title) = foreground_window();
        Some(PointerSample {
            x: pt.x,
            y: pt.y,
            left: down(VK_LBUTTON),
            right: down(VK_RBUTTON),
            middle: down(VK_MBUTTON),
            key: any_non_mouse_key(),
            window,
            title,
        })
    }
}

fn any_non_mouse_key() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    // 0x08..=0xFE skips mouse buttons (VK_LBUTTON..=VK_XBUTTON2).
    (8u16..=0xFE).any(|vk| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0)
}

fn foreground_window() -> (u64, String) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW};
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return (0, String::new());
    }
    let mut buf = [0u16; 256];
    let n = i32::try_from(buf.len()).unwrap_or(i32::MAX);
    let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), n) };
    let n = usize::try_from(n.max(0)).unwrap_or(0);
    let title = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
    (hwnd as usize as u64, title)
}
