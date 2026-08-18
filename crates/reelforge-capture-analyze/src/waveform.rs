//! Build [`WaveformSidecar`] peaks for a committed session.

use reelforge_capture_core::{MediaTime, Result};
use reelforge_capture_platform::{WAVEFORM_RATE, decode_mono_f32};
use reelforge_capture_store::{SessionStore, WaveformLeg, WaveformPeak, WaveformSidecar};
use std::path::Path;

/// How finely to bucket the envelope.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaveformOptions {
    /// Decode sample rate (waveform, not playback).
    pub sample_rate: u32,
    /// Width of one peak bucket in seconds.
    pub bucket_secs: f64,
}

impl Default for WaveformOptions {
    fn default() -> Self {
        Self {
            sample_rate: WAVEFORM_RATE,
            bucket_secs: 0.05,
        }
    }
}

/// Outcome of [`session_waveforms`].
#[derive(Debug, Clone, PartialEq)]
pub struct WaveformMaterialization {
    /// Written sidecar.
    pub sidecar: WaveformSidecar,
    /// Legs that produced at least one bucket.
    pub measured: usize,
    /// Legs / segments ffmpeg could not decode.
    pub failures: Vec<String>,
}

/// Decode every configured audio leg and write `waveforms.json`.
///
/// Prefers the demuxed per-leg file when `audio.json` has one (no stream
/// index to guess). Otherwise reads `0:a:N` out of the muxed segment.
/// Times are shifted onto the session clock.
///
/// # Errors
///
/// Session I/O (writing the sidecar). A missing ffmpeg yields an empty
/// sidecar, not an error.
pub fn session_waveforms(
    store: &SessionStore,
    opts: &WaveformOptions,
) -> Result<WaveformMaterialization> {
    let mix = store.manifest().meta.spec.audio.clone();
    let segments = store.manifest().segments.clone();
    let sidecar = store.read_audio_sidecar()?;
    let mut out = WaveformMaterialization {
        sidecar: WaveformSidecar::new(opts.sample_rate.max(1), opts.bucket_secs.max(1e-3)),
        measured: 0,
        failures: Vec::new(),
    };
    if segments.is_empty() || !mix.has_any() {
        store.write_waveforms(&out.sidecar)?;
        return Ok(out);
    }

    for (leg, _) in mix.configured() {
        let mut peaks = Vec::new();
        let index = mix.audio_index(leg);
        for seg in &segments {
            let (path, mapped) = sidecar
                .as_ref()
                .and_then(|s| s.leg(leg))
                .and_then(|t| t.files.iter().find(|f| f.segment == seg.id))
                .map_or_else(
                    || (store.root().join(&seg.path), index),
                    |f| (store.root().join(&f.path), None),
                );
            if !path.is_file() {
                out.failures
                    .push(format!("{}: missing {}", leg.as_str(), path.display()));
                continue;
            }
            match decode_mono_f32(&path, mapped, out.sidecar.sample_rate) {
                Ok(Some(pcm)) => {
                    peaks.extend(bucket_peaks(
                        &pcm,
                        out.sidecar.sample_rate,
                        out.sidecar.bucket_secs,
                        seg.start,
                    ));
                }
                Ok(None) => out.failures.push(format!(
                    "{} {}: decode skipped",
                    leg.as_str(),
                    Path::new(&seg.path).display()
                )),
                Err(e) => out.failures.push(format!("{}: {e}", leg.as_str())),
            }
        }
        if !peaks.is_empty() {
            out.measured += 1;
        }
        out.sidecar.legs.push(WaveformLeg { leg, peaks });
    }
    store.write_waveforms(&out.sidecar)?;
    Ok(out)
}

/// Fold mono PCM into min/max buckets starting at `origin` on the session clock.
#[must_use]
pub fn bucket_peaks(
    samples: &[f32],
    sample_rate: u32,
    bucket_secs: f64,
    origin: MediaTime,
) -> Vec<WaveformPeak> {
    let rate = sample_rate.max(1);
    let width = bucket_secs.max(1e-3);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let per_bucket = ((f64::from(rate) * width).round() as usize).max(1);
    let scale = origin.timescale.max(1);
    let mut out = Vec::new();
    for (i, chunk) in samples.chunks(per_bucket).enumerate() {
        if chunk.is_empty() {
            continue;
        }
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for s in chunk {
            min = min.min(*s);
            max = max.max(*s);
        }
        if !min.is_finite() || !max.is_finite() {
            continue;
        }
        let i = i64::try_from(i).unwrap_or(i64::MAX);
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let start_ticks = origin.ticks + ((i as f64) * width * f64::from(scale)).round() as i64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let end_ticks = origin.ticks + (((i + 1) as f64) * width * f64::from(scale)).round() as i64;
        out.push(WaveformPeak {
            t0: MediaTime {
                ticks: start_ticks,
                timescale: scale,
            },
            t1: MediaTime {
                ticks: end_ticks.max(start_ticks + 1),
                timescale: scale,
            },
            min,
            max,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelforge_capture_core::HZ_1K;

    #[test]
    fn buckets_a_known_envelope() {
        // 8000 Hz, 0.05 s → 400 samples per bucket.
        let mut pcm = vec![0.1_f32; 400];
        pcm.extend(std::iter::repeat_n(-0.8, 400));
        let peaks = bucket_peaks(&pcm, 8_000, 0.05, MediaTime::zero(HZ_1K));
        assert_eq!(peaks.len(), 2);
        assert!((peaks[0].max - 0.1).abs() < 1e-5);
        assert!((peaks[1].min + 0.8).abs() < 1e-5);
        assert!((peaks[0].t1.as_secs() - 0.05).abs() < 1e-9);
        assert!((peaks[1].t0.as_secs() - 0.05).abs() < 1e-9);
    }

    #[test]
    fn empty_pcm_is_no_peaks() {
        assert!(bucket_peaks(&[], 8_000, 0.05, MediaTime::zero(HZ_1K)).is_empty());
    }
}
