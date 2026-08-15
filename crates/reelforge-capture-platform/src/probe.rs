//! Host `ffprobe` duration (no libav).

use reelforge_capture_core::{CaptureError, HZ_1K, MediaTime, Result};
use std::path::Path;
use std::process::Command;

/// `format.duration` seconds, if ffprobe can read the file.
///
/// # Errors
///
/// Spawn / I/O. Missing binary or unreadable file → `Ok(None)`.
pub fn probe_duration(path: impl AsRef<Path>) -> Result<Option<MediaTime>> {
    let program = std::env::var("REELFORGE_FFPROBE").unwrap_or_else(|_| "ffprobe".into());
    let out = match Command::new(&program)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path.as_ref())
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CaptureError::io(format!("ffprobe: {e}"))),
    };
    if !out.status.success() {
        return Ok(None);
    }
    parse_duration_secs(&String::from_utf8_lossy(&out.stdout))
}

/// First audio stream duration, if present.
///
/// # Errors
///
/// Spawn / I/O.
pub fn probe_audio_duration(path: impl AsRef<Path>) -> Result<Option<MediaTime>> {
    let program = std::env::var("REELFORGE_FFPROBE").unwrap_or_else(|_| "ffprobe".into());
    let out = match Command::new(&program)
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path.as_ref())
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CaptureError::io(format!("ffprobe audio: {e}"))),
    };
    if !out.status.success() {
        return Ok(None);
    }
    parse_duration_secs(&String::from_utf8_lossy(&out.stdout))
}

/// Parse a single `ffprobe` duration line.
pub fn parse_duration_secs(raw: &str) -> Result<Option<MediaTime>> {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && *l != "N/A");
    let Some(line) = line else {
        return Ok(None);
    };
    let secs: f64 = line
        .parse()
        .map_err(|_| CaptureError::message(format!("ffprobe duration: {line}")))?;
    if !(secs.is_finite() && secs > 0.0) {
        return Ok(None);
    }
    Ok(Some(MediaTime::from_secs(secs, HZ_1K)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_seconds() {
        let t = parse_duration_secs("5.012000\n").unwrap().unwrap();
        assert!((t.as_secs() - 5.012).abs() < 1e-6);
    }

    #[test]
    fn skips_na() {
        assert!(parse_duration_secs("N/A\n").unwrap().is_none());
        assert!(parse_duration_secs("\n").unwrap().is_none());
    }
}
