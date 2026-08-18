//! Host cursor / button / key / foreground-window sample.
//!
//! Windows uses `GetCursorPos` / `GetAsyncKeyState`. macOS uses CoreGraphics.
//! Linux uses X11 via `dlopen` (Wayland-only sessions yield `None`).

#[allow(dead_code)] // linux / macOS collectors; unit tests cover the helpers
mod decode;

#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
mod x11;

/// One poll of the system pointer / keyboard / foreground window.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)]
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
    /// Any non-mouse key is down.
    pub key: bool,
    /// Foreground window handle (`0` if unknown).
    pub window: u64,
    /// Foreground window title (only filled when the handle is known).
    pub title: String,
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

/// Host collector for the compile target.
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
type HostInner = windows::Collector;

#[cfg(target_os = "macos")]
type HostInner = macos::Collector;

#[cfg(target_os = "linux")]
type HostInner = x11::Collector;

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
#[derive(Debug, Default)]
struct HostInner;

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
impl HostInner {
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
        self.next.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_yields_nothing() {
        assert!(NullPointer.sample().is_none());
    }

    #[test]
    #[cfg(windows)]
    fn windows_collector_returns_a_sample() {
        assert!(HostPointer::default().sample().is_some());
    }
}
