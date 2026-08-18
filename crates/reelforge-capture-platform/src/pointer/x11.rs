//! Linux X11 collector via `dlopen("libX11.so.6")`.
//!
//! Wayland-only sessions (no XWayland, no `DISPLAY`) return `None`. The crate
//! still links without libx11-dev.

use super::PointerSample;
use super::decode::{buttons_from_mask, keymap_any_down};
use std::ffi::{CStr, c_char, c_int, c_uint, c_ulong, c_void};
use std::ptr;

type Display = c_void;
type Window = c_ulong;

type XOpenDisplayFn = unsafe extern "C" fn(*const c_char) -> *mut Display;
type XCloseDisplayFn = unsafe extern "C" fn(*mut Display) -> c_int;
type XDefaultScreenFn = unsafe extern "C" fn(*mut Display) -> c_int;
type XRootWindowFn = unsafe extern "C" fn(*mut Display, c_int) -> Window;
type XQueryPointerFn = unsafe extern "C" fn(
    *mut Display,
    Window,
    *mut Window,
    *mut Window,
    *mut c_int,
    *mut c_int,
    *mut c_int,
    *mut c_int,
    *mut c_uint,
) -> c_int;
type XQueryKeymapFn = unsafe extern "C" fn(*mut Display, *mut c_char) -> c_int;
type XGetInputFocusFn = unsafe extern "C" fn(*mut Display, *mut Window, *mut c_int) -> c_int;
type XFetchNameFn = unsafe extern "C" fn(*mut Display, Window, *mut *mut c_char) -> c_int;
type XFreeFn = unsafe extern "C" fn(*mut c_void) -> c_int;

struct Api {
    handle: *mut c_void,
    open: XOpenDisplayFn,
    close: XCloseDisplayFn,
    default_screen: XDefaultScreenFn,
    root_window: XRootWindowFn,
    query_pointer: XQueryPointerFn,
    query_keymap: XQueryKeymapFn,
    get_input_focus: XGetInputFocusFn,
    fetch_name: XFetchNameFn,
    free: XFreeFn,
}

pub struct Collector {
    api: Option<Api>,
    dpy: *mut Display,
}

impl std::fmt::Debug for Collector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("X11Collector")
            .field("connected", &(!self.dpy.is_null()))
            .finish()
    }
}

impl Default for Collector {
    fn default() -> Self {
        Self::connect()
    }
}

impl Collector {
    fn connect() -> Self {
        let Some(api) = load_x11() else {
            return Self {
                api: None,
                dpy: ptr::null_mut(),
            };
        };
        let dpy = unsafe { (api.open)(ptr::null()) };
        if dpy.is_null() {
            drop_lib(api.handle);
            return Self {
                api: None,
                dpy: ptr::null_mut(),
            };
        }
        Self {
            api: Some(api),
            dpy,
        }
    }

    pub fn sample(&mut self) -> Option<PointerSample> {
        let api = self.api.as_ref()?;
        if self.dpy.is_null() {
            return None;
        }
        let screen = unsafe { (api.default_screen)(self.dpy) };
        let root = unsafe { (api.root_window)(self.dpy, screen) };
        let mut root_ret: Window = 0;
        let mut child: Window = 0;
        let mut rx = 0;
        let mut ry = 0;
        let mut wx = 0;
        let mut wy = 0;
        let mut mask: c_uint = 0;
        let ok = unsafe {
            (api.query_pointer)(
                self.dpy,
                root,
                &raw mut root_ret,
                &raw mut child,
                &raw mut rx,
                &raw mut ry,
                &raw mut wx,
                &raw mut wy,
                &raw mut mask,
            )
        };
        if ok == 0 {
            return None;
        }
        let (left, right, middle) = buttons_from_mask(mask);
        let mut keys = [0i8; 32];
        unsafe {
            (api.query_keymap)(self.dpy, keys.as_mut_ptr());
        }
        let keys = keys.map(|b| b.cast_unsigned());
        let (window, title) = focus_window(api, self.dpy);
        Some(PointerSample {
            x: rx,
            y: ry,
            left,
            right,
            middle,
            key: keymap_any_down(&keys),
            window,
            title,
        })
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        if let Some(api) = self.api.take() {
            if !self.dpy.is_null() {
                unsafe {
                    (api.close)(self.dpy);
                }
                self.dpy = ptr::null_mut();
            }
            drop_lib(api.handle);
        }
    }
}

fn focus_window(api: &Api, dpy: *mut Display) -> (u64, String) {
    let mut focus: Window = 0;
    let mut revert = 0;
    unsafe {
        (api.get_input_focus)(dpy, &raw mut focus, &raw mut revert);
    }
    // None = 0, PointerRoot = 1 — not a real window.
    if focus <= 1 {
        return (0, String::new());
    }
    let mut name: *mut c_char = ptr::null_mut();
    let status = unsafe { (api.fetch_name)(dpy, focus, &raw mut name) };
    let title = if status != 0 && !name.is_null() {
        let s = unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            (api.free)(name.cast());
        }
        s
    } else {
        String::new()
    };
    (focus, title)
}

fn load_x11() -> Option<Api> {
    for name in [c"libX11.so.6", c"libX11.so"] {
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
        if handle.is_null() {
            continue;
        }
        if let Some(api) = load_api(handle) {
            return Some(api);
        }
        drop_lib(handle);
    }
    None
}

fn load_api(handle: *mut c_void) -> Option<Api> {
    let load = |sym: &CStr| -> Option<*mut c_void> {
        let p = unsafe { libc::dlsym(handle, sym.as_ptr()) };
        (!p.is_null()).then_some(p)
    };
    Some(Api {
        handle,
        open: cast_fn(load(c"XOpenDisplay")?)?,
        close: cast_fn(load(c"XCloseDisplay")?)?,
        default_screen: cast_fn(load(c"XDefaultScreen")?)?,
        root_window: cast_fn(load(c"XRootWindow")?)?,
        query_pointer: cast_fn(load(c"XQueryPointer")?)?,
        query_keymap: cast_fn(load(c"XQueryKeymap")?)?,
        get_input_focus: cast_fn(load(c"XGetInputFocus")?)?,
        fetch_name: cast_fn(load(c"XFetchName")?)?,
        free: cast_fn(load(c"XFree")?)?,
    })
}

#[allow(clippy::missing_transmute_annotations, clippy::transmute_ptr_to_ptr)]
fn cast_fn<T>(p: *mut c_void) -> Option<T> {
    if p.is_null() {
        return None;
    }
    Some(unsafe { std::mem::transmute_copy(&p) })
}

fn drop_lib(handle: *mut c_void) {
    if !handle.is_null() {
        unsafe {
            libc::dlclose(handle);
        }
    }
}
