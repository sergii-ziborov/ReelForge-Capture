//! Host `ffprobe` duration (no libav).

use reelforge_capture_core::{CaptureError, HZ_1K, MediaTime, Result};
use std::path::Path;
use std::process::Command;

/// One audio stream's clock (`a:N` among audio streams, not the mux index).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioStreamClock {
    /// `0:a:N` index.
    pub index: u32,
    /// Stream duration, when ffprobe reports it.
    pub duration: Option<MediaTime>,
    /// `start_time` (AAC priming / device delay).
    pub start: Option<MediaTime>,
}

/// Video + every audio stream + container duration from one ffprobe.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MediaClocks {
    /// `format.duration`.
    pub format: Option<MediaTime>,
    /// First video stream duration.
    pub video: Option<MediaTime>,
    /// First video `start_time`.
    pub video_start: Option<MediaTime>,
    /// Audio streams in encode order (`a:0`, `a:1`, …).
    pub audio: Vec<AudioStreamClock>,
}

impl MediaClocks {
    /// Duration that may own the session timeline.
    ///
    /// Video stream first. Container duration is used only when it is **not**
    /// just the longest audio stream (otherwise the picture would stretch to
    /// follow the mic).
    #[must_use]
    pub fn video_master(&self) -> Option<MediaTime> {
        if let Some(v) = self.video {
            return Some(v);
        }
        let format = self.format?;
        let longest_audio = self
            .audio
            .iter()
            .filter_map(|a| a.duration)
            .max_by_key(|d| d.ticks);
        if let Some(audio) = longest_audio
            && (audio.as_secs() - format.as_secs()).abs() < 0.05
        {
            return None;
        }
        Some(format)
    }
}

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

/// Probe video, every audio stream, and the container in one ffprobe call.
///
/// # Errors
///
/// Spawn / I/O. Missing binary or unreadable file → empty [`MediaClocks`].
pub fn probe_media_clocks(path: impl AsRef<Path>) -> Result<MediaClocks> {
    let program = std::env::var("REELFORGE_FFPROBE").unwrap_or_else(|_| "ffprobe".into());
    let out = match Command::new(&program)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration:stream=index,codec_type,duration,start_time",
            "-of",
            "json",
        ])
        .arg(path.as_ref())
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(MediaClocks::default()),
        Err(e) => return Err(CaptureError::io(format!("ffprobe clocks: {e}"))),
    };
    if !out.status.success() {
        return Ok(MediaClocks::default());
    }
    parse_media_clocks_json(&String::from_utf8_lossy(&out.stdout))
}

/// Parse `ffprobe -of json` output from [`probe_media_clocks`].
///
/// # Errors
///
/// Invalid JSON.
pub fn parse_media_clocks_json(raw: &str) -> Result<MediaClocks> {
    let v: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| CaptureError::message(format!("ffprobe clocks json: {e}")))?;
    let format = v
        .get("format")
        .and_then(|f| f.get("duration"))
        .and_then(json_secs);
    let mut video = None;
    let mut video_start = None;
    let mut audio = Vec::new();
    let mut audio_i = 0u32;
    if let Some(streams) = v.get("streams").and_then(|s| s.as_array()) {
        for s in streams {
            let kind = s.get("codec_type").and_then(|k| k.as_str()).unwrap_or("");
            let duration = s.get("duration").and_then(json_secs);
            let start = s.get("start_time").and_then(json_secs);
            match kind {
                "video" if video.is_none() => {
                    video = duration;
                    video_start = start;
                }
                "audio" => {
                    audio.push(AudioStreamClock {
                        index: audio_i,
                        duration,
                        start,
                    });
                    audio_i = audio_i.saturating_add(1);
                }
                _ => {}
            }
        }
    }
    Ok(MediaClocks {
        format,
        video,
        video_start,
        audio,
    })
}

fn json_secs(v: &serde_json::Value) -> Option<MediaTime> {
    let raw = match v {
        serde_json::Value::String(s) => s.as_str(),
        serde_json::Value::Number(n) => {
            return n
                .as_f64()
                .and_then(|secs| MediaTime::from_secs(secs, HZ_1K).ok())
                .filter(|t| t.ticks > 0);
        }
        _ => return None,
    };
    parse_duration_secs(raw).ok().flatten()
}

/// First video stream `width` × `height`.
///
/// # Errors
///
/// Spawn / I/O.
pub fn probe_video_size(path: impl AsRef<Path>) -> Result<Option<(u32, u32)>> {
    let program = std::env::var("REELFORGE_FFPROBE").unwrap_or_else(|_| "ffprobe".into());
    let out = match Command::new(&program)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(path.as_ref())
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CaptureError::io(format!("ffprobe size: {e}"))),
    };
    if !out.status.success() {
        return Ok(None);
    }
    parse_wxh(&String::from_utf8_lossy(&out.stdout))
}

/// `W,H` or `W,H\n` from ffprobe csv.
pub fn parse_wxh(raw: &str) -> Result<Option<(u32, u32)>> {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty());
    let Some(line) = line else {
        return Ok(None);
    };
    let mut parts = line.split([',', 'x', ' ']).filter(|p| !p.is_empty());
    let (Some(w), Some(h)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    let w: u32 = w
        .parse()
        .map_err(|_| CaptureError::message(format!("ffprobe size w: {line}")))?;
    let h: u32 = h
        .parse()
        .map_err(|_| CaptureError::message(format!("ffprobe size h: {line}")))?;
    if w == 0 || h == 0 {
        return Ok(None);
    }
    Ok(Some((w, h)))
}

/// Crop `input[start, start+duration)` to `crop` and scale back to `scale_to`.
///
/// # Errors
///
/// ffmpeg spawn / non-zero exit.
pub fn render_crop_scale(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    start: f64,
    duration: f64,
    crop: (u32, u32, u32, u32),
    scale_to: (u32, u32),
) -> Result<()> {
    let program = std::env::var("REELFORGE_FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
    let vf = format!(
        "crop={}:{}:{}:{},scale={}:{}:flags=lanczos+accurate_rnd+full_chroma_int,cas=strength=0.35",
        crop.2, crop.3, crop.0, crop.1, scale_to.0, scale_to.1
    );
    let status = Command::new(&program)
        .args(["-hide_banner", "-y", "-loglevel", "error"])
        .args(["-ss", &format!("{start}"), "-t", &format!("{duration}")])
        .arg("-i")
        .arg(input.as_ref())
        .args(["-vf", &vf, "-an", "-c:v", "libx264", "-preset", "ultrafast"])
        .arg(output.as_ref())
        .status()
        .map_err(|e| CaptureError::io(format!("ffmpeg crop: {e}")))?;
    if !status.success() {
        return Err(CaptureError::io("ffmpeg crop failed"));
    }
    Ok(())
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

    #[test]
    fn parses_csv_size() {
        assert_eq!(parse_wxh("320,180\n").unwrap(), Some((320, 180)));
        assert_eq!(parse_wxh("\n").unwrap(), None);
    }

    #[test]
    fn parses_stream_clocks_and_skips_audio_led_container() {
        let clocks = parse_media_clocks_json(
            r#"{
              "streams": [
                {"index": 0, "codec_type": "video", "duration": "5.000000", "start_time": "0.000000"},
                {"index": 1, "codec_type": "audio", "duration": "4.940000", "start_time": "0.021333"},
                {"index": 2, "codec_type": "audio", "duration": "5.010000", "start_time": "0.000000"}
              ],
              "format": {"duration": "5.010000"}
            }"#,
        )
        .unwrap();
        assert!((clocks.video.unwrap().as_secs() - 5.0).abs() < 1e-6);
        assert_eq!(clocks.audio.len(), 2);
        assert_eq!(clocks.audio[0].index, 0);
        assert!(
            (clocks.audio[0].start.unwrap().as_secs() - 0.021).abs() < 1e-3,
            "1 kHz clock keeps milliseconds, not microseconds"
        );
        assert!((clocks.video_master().unwrap().as_secs() - 5.0).abs() < 1e-6);

        let audio_only = parse_media_clocks_json(
            r#"{
              "streams": [
                {"index": 0, "codec_type": "audio", "duration": "5.000000", "start_time": "0.000000"}
              ],
              "format": {"duration": "5.000000"}
            }"#,
        )
        .unwrap();
        assert!(audio_only.video_master().is_none());
    }
}
