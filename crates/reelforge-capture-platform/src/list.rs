//! Enumerate windows / audio without linking libav.

use crate::host::HostOs;
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

/// Raw device dump from the host audio backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioListing {
    /// Backend that was queried.
    pub backend: String,
    /// stderr/stdout (may be empty if ffmpeg is missing).
    pub raw: String,
}

/// List windows on this host.
///
/// # Errors
///
/// Process spawn / wait.
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    list_windows_on(HostOs::current())
}

/// List windows for `os` (tests inject a host).
///
/// # Errors
///
/// Process spawn / wait.
pub fn list_windows_on(os: HostOs) -> Result<Vec<WindowInfo>> {
    match os {
        HostOs::Windows => list_windows_powershell(),
        HostOs::Macos => list_windows_osascript(),
        HostOs::Linux => Ok(list_windows_wmctrl()),
    }
}

/// Ask the host audio backend for device names. Best-effort.
///
/// # Errors
///
/// Spawn (missing tool is `Ok` with empty `raw`).
pub fn list_audio_hint() -> Result<AudioListing> {
    list_audio_hint_on(HostOs::current())
}

/// Audio listing for `os`.
///
/// # Errors
///
/// Spawn.
pub fn list_audio_hint_on(os: HostOs) -> Result<AudioListing> {
    Ok(match os {
        HostOs::Windows => ffmpeg_list("dshow"),
        HostOs::Macos => ffmpeg_list("avfoundation"),
        HostOs::Linux => pulse_or_ffmpeg(),
    })
}

/// Parse `process<TAB>title` lines (PowerShell / osascript helpers).
#[must_use]
pub fn parse_tab_windows(text: &str) -> Vec<WindowInfo> {
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
    rows
}

/// Parse `wmctrl -l` (`id desktop host title…`).
#[must_use]
pub fn parse_wmctrl(text: &str) -> Vec<WindowInfo> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let mut bits = line.split_whitespace();
        let _id = bits.next();
        let _desk = bits.next();
        let host = bits.next().unwrap_or("").to_string();
        let title = bits.collect::<Vec<_>>().join(" ");
        if title.is_empty() {
            continue;
        }
        rows.push(WindowInfo {
            process: host,
            title,
        });
    }
    rows
}

fn list_windows_powershell() -> Result<Vec<WindowInfo>> {
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-Process | Where-Object { $_.MainWindowTitle } | ForEach-Object { '{0}\t{1}' -f $_.ProcessName, $_.MainWindowTitle }",
        ])
        .output()
        .map_err(|e| reelforge_capture_core::CaptureError::io(format!("powershell: {e}")))?;
    Ok(parse_tab_windows(&String::from_utf8_lossy(&out.stdout)))
}

fn list_windows_osascript() -> Result<Vec<WindowInfo>> {
    let script = r#"tell application "System Events"
set out to ""
repeat with p in (every process whose background only is false)
repeat with w in windows of p
set out to out & name of p & tab & name of w & linefeed
end repeat
end repeat
return out
end tell"#;
    let out = Command::new("osascript")
        .args(["-e", script])
        .output()
        .map_err(|e| reelforge_capture_core::CaptureError::io(format!("osascript: {e}")))?;
    Ok(parse_tab_windows(&String::from_utf8_lossy(&out.stdout)))
}

fn list_windows_wmctrl() -> Vec<WindowInfo> {
    match Command::new("wmctrl").arg("-l").output() {
        Ok(o) => parse_wmctrl(&String::from_utf8_lossy(&o.stdout)),
        Err(_) => Vec::new(),
    }
}

fn ffmpeg_list(backend: &str) -> AudioListing {
    let program = std::env::var("REELFORGE_FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
    let dummy = if backend == "avfoundation" {
        ""
    } else {
        "dummy"
    };
    let out = Command::new(program)
        .args([
            "-hide_banner",
            "-list_devices",
            "true",
            "-f",
            backend,
            "-i",
            dummy,
        ])
        .output();
    AudioListing {
        backend: backend.into(),
        raw: match out {
            Ok(o) => {
                let mut raw = String::from_utf8_lossy(&o.stderr).into_owned();
                raw.push_str(&String::from_utf8_lossy(&o.stdout));
                raw
            }
            Err(_) => String::new(),
        },
    }
}

fn pulse_or_ffmpeg() -> AudioListing {
    if let Ok(o) = Command::new("pactl")
        .args(["list", "short", "sources"])
        .output()
        && o.status.success()
    {
        return AudioListing {
            backend: "pulse".into(),
            raw: String::from_utf8_lossy(&o.stdout).into_owned(),
        };
    }
    ffmpeg_list("pulse")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tab_and_wmctrl() {
        let w = parse_tab_windows("Safari\tInbox\n");
        assert_eq!(w[0].process, "Safari");
        assert_eq!(w[0].title, "Inbox");
        let x = parse_wmctrl("0x1  0  host  Foo Bar\n");
        assert_eq!(x[0].title, "Foo Bar");
    }
}
