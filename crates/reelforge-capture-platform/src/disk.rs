//! Free space on the session volume.

use std::path::Path;

/// Bytes free on the volume that contains `path`. `None` if the host cannot say.
#[must_use]
pub fn available_bytes(path: &Path) -> Option<u64> {
    available_bytes_impl(path)
}

#[cfg(windows)]
fn available_bytes_impl(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut free: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &raw mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

#[cfg(unix)]
fn available_bytes_impl(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    let c = CString::new(path.to_string_lossy().as_bytes()).ok()?;
    let mut s = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let rc = unsafe { libc::statvfs(c.as_ptr(), s.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let s = unsafe { s.assume_init() };
    // `fsblkcnt_t` / `f_frsize` are u32 on some Unix targets and u64 on others.
    // A generic widen avoids `try_from` (identity on Linux, infallible on macOS).
    Some(widen_u64(s.f_bavail).saturating_mul(widen_u64(s.f_frsize)))
}

#[cfg(unix)]
fn widen_u64(value: impl Into<u64>) -> u64 {
    value.into()
}

#[cfg(not(any(windows, unix)))]
fn available_bytes_impl(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dir_reports_some_space() {
        let free = available_bytes(std::env::temp_dir().as_path());
        if let Some(n) = free {
            assert!(n > 0);
        }
    }
}
