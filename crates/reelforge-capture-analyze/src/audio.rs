//! Demux each configured audio leg into its own file.

use reelforge_capture_core::{AudioLeg, MediaTime, Result, SegmentId};
use reelforge_capture_platform::{extract_audio_stream, extract_audio_streams, probe_duration};
use reelforge_capture_store::{
    AudioLegTrack, AudioSegmentFile, AudioSidecar, ClockSidecar, SessionStore,
};
use std::fs;
use std::path::{Path, PathBuf};

/// Container for demuxed legs. Capture always encodes AAC, so an MP4 audio
/// container takes the copied stream without a re-encode.
const AUDIO_EXT: &str = "m4a";

/// Outcome of [`materialize_audio`].
#[derive(Debug, Clone, PartialEq)]
pub struct AudioMaterialization {
    /// Files that now exist, ready to be referenced by a project.
    pub sidecar: AudioSidecar,
    /// Segments demuxed during this call.
    pub extracted: usize,
    /// Segments already on disk from an earlier call.
    pub reused: usize,
    /// Segments that could not be demuxed (host ffmpeg said why).
    pub failures: Vec<String>,
}

/// Demux every configured audio leg of every committed segment.
///
/// Writes `audio/<leg>/<segment>.m4a` plus `audio.json`, and returns what
/// happened. Already-extracted files are kept (committed segments are
/// immutable), so calling this twice is cheap.
///
/// All legs of a segment come out of **one** ffmpeg invocation — the file is
/// read once. If that fails, each leg is retried on its own, so a stream the
/// device never opened does not cost the legs that recorded fine. A leg that
/// fails for one segment keeps its other segments: the sidecar then describes
/// a real hole instead of pretending the audio is continuous.
///
/// # Errors
///
/// Session I/O (creating the audio directories, writing the sidecar).
pub fn materialize_audio(store: &SessionStore) -> Result<AudioMaterialization> {
    let mix = store.manifest().meta.spec.audio.clone();
    let segments = store.manifest().segments.clone();
    let clocks = store.read_clocks()?.unwrap_or_default();
    let mut out = AudioMaterialization {
        sidecar: AudioSidecar::new(),
        extracted: 0,
        reused: 0,
        failures: Vec::new(),
    };

    let legs: Vec<(AudioLeg, u32, String)> = mix
        .configured()
        .iter()
        .filter_map(|(leg, device)| {
            mix.audio_index(*leg)
                .map(|index| (*leg, index, device.name.clone()))
        })
        .collect();
    let mut tracks: Vec<AudioLegTrack> = legs
        .iter()
        .map(|(leg, index, device)| AudioLegTrack {
            leg: *leg,
            device: device.clone(),
            audio_index: *index,
            files: Vec::new(),
        })
        .collect();
    for (leg, _, _) in &legs {
        fs::create_dir_all(store.root().join("audio").join(leg.as_str()))?;
    }

    for seg in &segments {
        let src = store.root().join(&seg.path);
        let mut pending: Vec<(u32, PathBuf)> = Vec::new();
        for (leg, index, _) in &legs {
            let dst = store.root().join(audio_rel_path(*leg, seg.id));
            if written(&dst) {
                out.reused += 1;
            } else {
                pending.push((*index, dst));
            }
        }
        if !pending.is_empty() && extract_audio_streams(&src, &pending).is_err() {
            // One unreadable stream must not cost the others.
            for (index, dst) in &pending {
                if let Err(e) = extract_audio_stream(&src, *index, dst) {
                    out.failures.push(e.to_string());
                }
            }
        }
        for (slot, (leg, index, _)) in legs.iter().enumerate() {
            let rel = audio_rel_path(*leg, seg.id);
            let dst = store.root().join(&rel);
            if !written(&dst) {
                continue; // a hole; the failure is already recorded
            }
            if pending.iter().any(|(_, p)| *p == dst) {
                out.extracted += 1;
            }
            let scale = seg.start.timescale.max(1);
            let duration = probe_duration(&dst)
                .ok()
                .flatten()
                .or_else(|| clock_leg_duration(&clocks, seg.id, *index, scale));
            tracks[slot].files.push(AudioSegmentFile {
                segment: seg.id,
                path: rel,
                start: seg.start,
                end: seg.end,
                duration,
                gap: duration.map(|d| MediaTime {
                    ticks: d.ticks - (seg.end.ticks - seg.start.ticks),
                    timescale: scale,
                }),
            });
        }
    }

    out.sidecar.legs = tracks;
    store.write_audio_sidecar(&out.sidecar)?;
    Ok(out)
}

/// Relative path of one leg's file for one segment.
#[must_use]
pub fn audio_rel_path(leg: AudioLeg, segment: SegmentId) -> String {
    format!("audio/{}/{}.{AUDIO_EXT}", leg.as_str(), segment.file_stem())
}

/// Whether a demuxed file is already on disk with content.
fn written(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.len() > 0)
}

fn clock_leg_duration(
    clocks: &ClockSidecar,
    segment: SegmentId,
    index: u32,
    scale: u32,
) -> Option<MediaTime> {
    let secs = clocks.segment(segment)?.audio_leg(index)?.duration_secs?;
    MediaTime::from_secs(secs, scale).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leg_paths_are_stable_and_padded() {
        assert_eq!(
            audio_rel_path(AudioLeg::System, SegmentId(7)),
            "audio/system/000007.m4a"
        );
        assert_eq!(
            audio_rel_path(AudioLeg::Microphone, SegmentId(1)),
            "audio/microphone/000001.m4a"
        );
    }
}
