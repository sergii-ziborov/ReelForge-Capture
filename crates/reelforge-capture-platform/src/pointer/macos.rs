//! macOS cursor / buttons / keys / front window (CoreGraphics, no AppKit).

use super::PointerSample;
use super::decode::front_window;
use std::ffi::{c_char, c_void};
use std::ptr;

type CfType = *const c_void;
type CfArray = *const c_void;
type CfDict = *const c_void;
type CfString = *const c_void;

const COMBINED_SESSION: i32 = 1;
const MOUSE_LEFT: u32 = 0;
const MOUSE_RIGHT: u32 = 1;
const MOUSE_CENTER: u32 = 2;
const ON_SCREEN_ONLY: u32 = 1;
const EXCLUDE_DESKTOP: u32 = 16;
const CF_NUMBER_SINT64: i32 = 4;
const CF_NUMBER_SINT32: i32 = 3;
const CF_UTF8: u32 = 0x0800_0100;

#[repr(C)]
struct CgPoint {
    x: f64,
    y: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    static kCGWindowNumber: CfString;
    static kCGWindowName: CfString;
    static kCGWindowLayer: CfString;
    static kCGWindowOwnerName: CfString;
    fn CGEventCreate(source: *mut c_void) -> *mut c_void;
    fn CGEventGetUnflippedLocation(event: *mut c_void) -> CgPoint;
    fn CGEventSourceButtonState(state_id: i32, button: u32) -> u8;
    fn CGEventSourceKeyState(state_id: i32, key: u16) -> u8;
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CfArray;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CfType);
    fn CFArrayGetCount(the_array: CfArray) -> isize;
    fn CFArrayGetValueAtIndex(the_array: CfArray, idx: isize) -> *const c_void;
    fn CFDictionaryGetValue(the_dict: CfDict, key: *const c_void) -> *const c_void;
    fn CFNumberGetValue(number: *const c_void, the_type: i32, value_ptr: *mut c_void) -> u8;
    fn CFStringGetCString(
        the_string: CfString,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> u8;
}

#[derive(Debug, Default)]
pub struct Collector;

impl Collector {
    #[allow(clippy::unused_self)]
    pub fn sample(&mut self) -> Option<PointerSample> {
        let event = unsafe { CGEventCreate(ptr::null_mut()) };
        if event.is_null() {
            return None;
        }
        let pt = unsafe { CGEventGetUnflippedLocation(event) };
        unsafe {
            CFRelease(event);
        }
        #[allow(clippy::cast_possible_truncation)]
        let x = pt.x.round() as i32;
        #[allow(clippy::cast_possible_truncation)]
        let y = pt.y.round() as i32;
        let down = |button: u32| unsafe { CGEventSourceButtonState(COMBINED_SESSION, button) } != 0;
        let (window, title) = frontmost();
        Some(PointerSample {
            x,
            y,
            left: down(MOUSE_LEFT),
            right: down(MOUSE_RIGHT),
            middle: down(MOUSE_CENTER),
            key: any_key(),
            window,
            title,
        })
    }
}

fn any_key() -> bool {
    (0u16..=126).any(|k| unsafe { CGEventSourceKeyState(COMBINED_SESSION, k) } != 0)
}

fn frontmost() -> (u64, String) {
    let list = unsafe { CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0) };
    if list.is_null() {
        return (0, String::new());
    }
    let rows = collect_windows(list);
    unsafe {
        CFRelease(list);
    }
    front_window(&rows).unwrap_or((0, String::new()))
}

fn collect_windows(list: CfArray) -> Vec<(i64, i32, String)> {
    let n = unsafe { CFArrayGetCount(list) };
    let mut out = Vec::new();
    let n = usize::try_from(n.max(0)).unwrap_or(0);
    for i in 0..n {
        let idx = isize::try_from(i).unwrap_or(0);
        let dict = unsafe { CFArrayGetValueAtIndex(list, idx) };
        if dict.is_null() {
            continue;
        }
        let layer =
            cf_i32(unsafe { CFDictionaryGetValue(dict, kCGWindowLayer.cast()) }).unwrap_or(-1);
        let id = cf_i64(unsafe { CFDictionaryGetValue(dict, kCGWindowNumber.cast()) }).unwrap_or(0);
        let mut title = cf_string(unsafe { CFDictionaryGetValue(dict, kCGWindowName.cast()) });
        if title.is_empty() {
            title = cf_string(unsafe { CFDictionaryGetValue(dict, kCGWindowOwnerName.cast()) });
        }
        out.push((id, layer, title));
    }
    out
}

fn cf_i32(v: *const c_void) -> Option<i32> {
    if v.is_null() {
        return None;
    }
    let mut n = 0i32;
    let ok = unsafe { CFNumberGetValue(v, CF_NUMBER_SINT32, (&raw mut n).cast()) };
    (ok != 0).then_some(n)
}

fn cf_i64(v: *const c_void) -> Option<i64> {
    if v.is_null() {
        return None;
    }
    let mut n = 0i64;
    let ok = unsafe { CFNumberGetValue(v, CF_NUMBER_SINT64, (&raw mut n).cast()) };
    (ok != 0).then_some(n)
}

fn cf_string(v: *const c_void) -> String {
    if v.is_null() {
        return String::new();
    }
    let mut buf = [0i8; 256];
    let ok = unsafe {
        CFStringGetCString(
            v,
            buf.as_mut_ptr(),
            isize::try_from(buf.len()).unwrap_or(256),
            CF_UTF8,
        )
    };
    if ok == 0 {
        return String::new();
    }
    let bytes = buf.map(|b| b.cast_unsigned());
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}
