//! Decode a host audio stream to mono `f32` PCM via ffmpeg (no libav).

use crate::signals::ffmpeg_program;
use reelforge_capture_core::{CaptureError, Result};
use std::path::Path;
use std::process::{Command, Stdio};

/// Sample rate used for waveform envelopes (UI, not playback).
pub const WAVEFORM_RATE: u32 = 8_000;

/// Decode one audio stream to interleaved-mono `f32` little-endian samples.
///
/// `audio_index` is the `0:a:N` slot inside a mux. `None` takes the default
/// audio stream (a demuxed single-leg file).
///
/// # Errors
///
/// Spawn / I/O. Missing ffmpeg or a missing stream → `Ok(None)`.
pub fn decode_mono_f32(
    path: impl AsRef<Path>,
    audio_index: Option<u32>,
    rate: u32,
) -> Result<Option<Vec<f32>>> {
    let program = ffmpeg_program();
    let rate = rate.max(1);
    let mut cmd = Command::new(&program);
    cmd.args(["-hide_banner", "-v", "error", "-i"])
        .arg(path.as_ref());
    if let Some(i) = audio_index {
        cmd.args(["-map", &format!("0:a:{i}")]);
    }
    cmd.args([
        "-vn",
        "-sn",
        "-dn",
        "-ac",
        "1",
        "-ar",
        &rate.to_string(),
        "-f",
        "f32le",
        "-",
    ])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CaptureError::io(format!("{program} pcm: {e}"))),
    };
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(f32le_samples(&out.stdout)))
}

/// Interpret a little-endian `f32` buffer. A trailing incomplete word is dropped.
#[must_use]
pub fn f32le_samples(bytes: &[u8]) -> Vec<f32> {
    let n = bytes.len() / 4;
    let mut out = Vec::with_capacity(n);
    for chunk in bytes.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_le_floats() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1.0f32.to_le_bytes());
        bytes.extend_from_slice(&(-0.5f32).to_le_bytes());
        bytes.push(0); // trailing junk
        let s = f32le_samples(&bytes);
        assert_eq!(s.len(), 2);
        assert!((s[0] - 1.0).abs() < 1e-6);
        assert!((s[1] + 0.5).abs() < 1e-6);
    }
}
