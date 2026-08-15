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
    #[allow(clippy::cast_possible_truncation)]
    let avail = s.f_bavail as u64;
    #[allow(clippy::cast_possible_truncation)]
    let fr = s.f_frsize as u64;
    Some(avail.saturating_mul(fr))
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
