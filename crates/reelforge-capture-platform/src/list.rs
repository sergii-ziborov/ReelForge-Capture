//! Enumerate windows / audio without linking libav.

use reelforge_capture_core::Result;
use std::process::Command;

/// Visible top-level window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    /// Process name if known.
    pub process: String,
    /// Title (used as `VideoSource::Window`).
    pub title: String,
}

/// Hint for dshow/wasapi names (textual ffmpeg dump).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioListing {
    /// Raw stderr/stdout from ffmpeg device list (may be empty).
    pub raw: String,
}

/// List windows with a title via `PowerShell` (Windows) or empty elsewhere.
///
/// # Errors
///
/// Process spawn / wait.
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    if !cfg!(windows) {
        return Ok(Vec::new());
    }
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-Process | Where-Object { $_.MainWindowTitle } | ForEach-Object { '{0}\t{1}' -f $_.ProcessName, $_.MainWindowTitle }",
        ])
        .output()
        .map_err(|e| reelforge_capture_core::CaptureError::io(format!("powershell: {e}")))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut rows = Vec::new();
    for line in text.lines() {
        let Some((proc, title)) = line.split_once('\t') else {
            continue;
        };
        let title = title.trim();
        if title.is_empty() {
            continue;
        }
        rows.push(WindowInfo {
            process: proc.trim().into(),
            title: title.into(),
        });
    }
    Ok(rows)
}

/// Ask ffmpeg to dump dshow devices (Windows). Best-effort; may be empty.
///
/// # Errors
///
/// Spawn.
pub fn list_audio_hint() -> Result<AudioListing> {
    let program = std::env::var("REELFORGE_FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
    let out = Command::new(program)
        .args([
            "-hide_banner",
            "-list_devices",
            "true",
            "-f",
            "dshow",
            "-i",
            "dummy",
        ])
        .output();
    match out {
        Ok(o) => {
            let mut raw = String::from_utf8_lossy(&o.stderr).into_owned();
            raw.push_str(&String::from_utf8_lossy(&o.stdout));
            Ok(AudioListing { raw })
        }
        Err(_) => Ok(AudioListing { raw: String::new() }),
    }
}
