//! Host cursor / button sample (Windows). Other hosts return `None`.

/// One poll of the system pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerSample {
    /// Desktop X.
    pub x: i32,
    /// Desktop Y.
    pub y: i32,
    /// Left button down.
    pub left: bool,
    /// Right button down.
    pub right: bool,
    /// Middle button down.
    pub middle: bool,
}

/// Something that can be polled for cursor position / buttons.
pub trait PointerSource {
    /// Current pointer, or `None` if this host cannot sample.
    fn sample(&mut self) -> Option<PointerSample>;
}

/// No-op (tests, or a host without a collector).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullPointer;

impl PointerSource for NullPointer {
    fn sample(&mut self) -> Option<PointerSample> {
        None
    }
}

/// Host collector. Implemented on Windows; elsewhere this is [`NullPointer`].
#[derive(Debug, Default)]
pub struct HostPointer {
    inner: HostInner,
}

impl PointerSource for HostPointer {
    fn sample(&mut self) -> Option<PointerSample> {
        self.inner.sample()
    }
}

#[cfg(windows)]
#[derive(Debug, Default)]
struct HostInner;

#[cfg(windows)]
impl HostInner {
    #[allow(clippy::unused_self)]
    fn sample(&mut self) -> Option<PointerSample> {
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
        Some(PointerSample {
            x: pt.x,
            y: pt.y,
            left: down(VK_LBUTTON),
            right: down(VK_RBUTTON),
            middle: down(VK_MBUTTON),
        })
    }
}

#[cfg(not(windows))]
#[derive(Debug, Default)]
struct HostInner;

#[cfg(not(windows))]
impl HostInner {
    #[allow(clippy::unused_self)]
    fn sample(&mut self) -> Option<PointerSample> {
        None
    }
}

/// Scripted samples for supervisor tests.
#[derive(Debug, Default)]
pub struct FakePointer {
    /// Next sample to yield (`None` = no collector this poll).
    pub next: Option<PointerSample>,
}

impl PointerSource for FakePointer {
    fn sample(&mut self) -> Option<PointerSample> {
        self.next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_yields_nothing() {
        assert!(NullPointer.sample().is_none());
    }
}
