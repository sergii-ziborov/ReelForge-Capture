//! Headless `CaptureProject` v1 authoring.
//!
//! The document shape lives in [`reelforge_capture_schema`] — the typed
//! contract shared with `ReelForge`. This crate only decides *what* to put in
//! it: which files become media, which kept ranges become clips, and how
//! audio legs are addressed.
//!
//! # Audio
//!
//! A Capture segment muxes every audio leg into one `.mkv`, and a project
//! clip cannot say "stream 2 of this file". So audio tracks reference the
//! **demuxed per-leg files** recorded in `audio.json`
//! (`reelforge_capture_analyze::materialize_audio`). Without that sidecar the
//! legs are still described — as **muted** tracks tagged
//! `audio_resolved=false` — because an unresolved audio track that plays is
//! worse than one that visibly does not.

use reelforge_capture_core::{AudioLeg, CaptureError, MediaTime, Result, SegmentId, SessionId};
use reelforge_capture_edit::{ClickZoom, KeptRange};
use reelforge_capture_schema::{
    CaptureProject, Marker, MediaRef, MediaRefId, Metadata, ProjectId, Retiming, SemanticRef,
    Sequence, SequenceId, SourceRange, TimelineClip, TimelineClipId, TimelineItem, TimelineTrack,
    TimelineTrackId, TrackKind,
};
use reelforge_capture_store::{AudioSidecar, SegmentRecord, SessionStore};

/// One file a clip can be cut from: a media entry plus its session span.
struct Source {
    media: MediaRefId,
    /// Start on the session clock.
    start: i64,
    /// End on the session clock.
    end: i64,
    /// Offset of the session start inside the file (`0` for segment files).
    file_base: i64,
    tags: Vec<(String, String)>,
}

/// Build a project from committed segments + kept ranges + optional click zooms.
///
/// Each committed segment is a video media entry. A kept range that spans two
/// segments becomes two clips. Audio legs become audio tracks over the
/// demuxed files from `audio.json` when it exists.
///
/// Click zooms are **markers only** — not crop/scale keyframes.
///
/// # Errors
///
/// No committed segments, an inconsistent document, or JSON.
pub fn project_from_session(
    store: &SessionStore,
    kept: &[KeptRange],
    zooms: &[ClickZoom],
) -> Result<CaptureProject> {
    let meta = &store.manifest().meta;
    let segments = &store.manifest().segments;
    if segments.is_empty() {
        return Err(CaptureError::message(
            "no committed segments; cannot emit a project (run a supervised capture first)",
        ));
    }
    let sidecar = store.read_audio_sidecar()?;

    let mut project = CaptureProject::new(ProjectId::new(meta.id.as_str()), meta.name.clone());

    let video_sources: Vec<Source> = segments
        .iter()
        .map(|s| Source {
            media: video_media_id(s),
            start: s.start.ticks,
            end: s.end.ticks,
            file_base: s.start.ticks,
            tags: Vec::new(),
        })
        .collect();
    project.media = segments
        .iter()
        .map(|s| MediaRef {
            id: video_media_id(s),
            uri: session_uri(store, &s.path),
            duration: Some(seg_duration(s)),
            role: Some("video".into()),
        })
        .collect();

    let mut tracks = vec![TimelineTrack {
        id: TimelineTrackId::new("v0"),
        kind: TrackKind::Video,
        items: clips_for_ranges(&video_sources, kept, "c"),
        muted: false,
    }];

    for (leg, _device) in meta.spec.audio.configured() {
        let plan = plan_audio_leg(store, segments, sidecar.as_ref(), leg);
        project.media.extend(plan.media);
        let mut track = TimelineTrack::new(audio_track_id(leg), TrackKind::Audio);
        track.items = clips_for_ranges(&plan.sources, kept, audio_clip_prefix(leg));
        track.muted = plan.muted;
        tracks.push(track);
    }

    let mut main = Sequence::new(SequenceId::new("main"), "main");
    main.tracks = tracks;
    main.markers = zoom_markers(zooms);
    project.sequences = vec![main];
    project.validate()?;
    Ok(project)
}

/// How one audio leg is addressed: demuxed files, or a flagged fallback.
struct AudioPlan {
    sources: Vec<Source>,
    media: Vec<MediaRef>,
    muted: bool,
}

fn plan_audio_leg(
    store: &SessionStore,
    segments: &[SegmentRecord],
    sidecar: Option<&AudioSidecar>,
    leg: AudioLeg,
) -> AudioPlan {
    if let Some(track) = sidecar
        .and_then(|s| s.leg(leg))
        .filter(|t| !t.files.is_empty())
    {
        let mut plan = AudioPlan {
            sources: Vec::new(),
            media: Vec::new(),
            muted: false,
        };
        for f in &track.files {
            let id = audio_media_id(leg, f.segment);
            let mut tags = vec![("audio_leg".to_string(), leg.as_str().to_string())];
            if let Some(ms) = f.gap_ms().filter(|ms| *ms != 0) {
                tags.push(("audio_gap_ms".into(), ms.to_string()));
            }
            plan.media.push(MediaRef {
                id: id.clone(),
                uri: session_uri(store, &f.path),
                duration: f.duration,
                role: Some("audio".into()),
            });
            plan.sources.push(Source {
                media: id,
                start: f.start.ticks,
                end: f.end.ticks,
                // A demuxed file starts at its segment boundary.
                file_base: f.start.ticks,
                tags,
            });
        }
        return plan;
    }

    // No demuxed file: describe the leg, but do not pretend it resolves.
    let stream = store.manifest().meta.spec.audio.stream_index(leg);
    AudioPlan {
        sources: segments
            .iter()
            .map(|s| Source {
                media: video_media_id(s),
                start: s.start.ticks,
                end: s.end.ticks,
                file_base: s.start.ticks,
                tags: vec![
                    ("audio_leg".into(), leg.as_str().to_string()),
                    ("audio_resolved".into(), "false".into()),
                    (
                        "audio_stream".into(),
                        stream.map_or_else(|| "?".into(), |i| i.to_string()),
                    ),
                ],
            })
            .collect(),
        media: Vec::new(),
        muted: true,
    }
}

fn zoom_markers(zooms: &[ClickZoom]) -> Vec<Marker> {
    zooms
        .iter()
        .enumerate()
        .map(|(i, z)| Marker {
            t: z.range.start,
            duration: Some(MediaTime {
                ticks: z.range.end.ticks - z.range.start.ticks,
                timescale: z.range.start.timescale,
            }),
            name: format!("click_zoom_{i}"),
            semantic: Some(SemanticRef::new("event", format!("click:{},{}", z.x, z.y))),
        })
        .collect()
}

/// Pretty JSON for `CaptureProject::from_json`.
///
/// # Errors
///
/// Serde.
pub fn to_json_pretty(project: &CaptureProject) -> Result<String> {
    project.to_json_pretty()
}

/// Configured audio legs this session cannot address yet (no demuxed files).
///
/// # Errors
///
/// Reading `audio.json`.
pub fn unresolved_audio(store: &SessionStore) -> Result<Vec<AudioLeg>> {
    let sidecar = store.read_audio_sidecar()?;
    Ok(store
        .manifest()
        .meta
        .spec
        .audio
        .configured()
        .iter()
        .map(|(leg, _)| *leg)
        .filter(|leg| {
            sidecar
                .as_ref()
                .and_then(|s| s.leg(*leg))
                .is_none_or(|t| t.files.is_empty())
        })
        .collect())
}

/// Convenience: project id for a session.
#[must_use]
pub fn session_project_id(id: &SessionId) -> String {
    id.as_str().to_string()
}

fn video_media_id(seg: &SegmentRecord) -> MediaRefId {
    MediaRefId::new(format!("seg_{}", seg.id.file_stem()))
}

fn audio_media_id(leg: AudioLeg, segment: SegmentId) -> MediaRefId {
    MediaRefId::new(format!("{}_{}", leg.as_str(), segment.file_stem()))
}

fn audio_track_id(leg: AudioLeg) -> TimelineTrackId {
    TimelineTrackId::new(match leg {
        AudioLeg::System => "a_system",
        AudioLeg::Microphone => "a_mic",
    })
}

const fn audio_clip_prefix(leg: AudioLeg) -> &'static str {
    match leg {
        AudioLeg::System => "as",
        AudioLeg::Microphone => "am",
    }
}

fn session_uri(store: &SessionStore, rel: &str) -> String {
    store.root().join(rel).to_string_lossy().replace('\\', "/")
}

fn seg_duration(seg: &SegmentRecord) -> MediaTime {
    MediaTime {
        ticks: (seg.end.ticks - seg.start.ticks).max(0),
        timescale: seg.start.timescale.max(1),
    }
}

/// Cut each kept range against each source file, in record order.
fn clips_for_ranges(sources: &[Source], kept: &[KeptRange], prefix: &str) -> Vec<TimelineItem> {
    let mut items = Vec::new();
    let mut n = 0u32;
    for k in kept {
        for src in sources {
            let start = k.source.start.ticks.max(src.start);
            let end = k.source.end.ticks.min(src.end);
            if end <= start {
                continue;
            }
            let scale = k.source.start.timescale.max(1);
            let retiming = if (k.speed - 1.0).abs() < 1e-9 {
                Retiming::Identity
            } else {
                Retiming::Speed { factor: k.speed }
            };
            items.push(TimelineItem::Clip(TimelineClip {
                id: TimelineClipId::new(format!("{prefix}{n}")),
                media: src.media.clone(),
                source: SourceRange {
                    start: MediaTime {
                        ticks: start - src.file_base,
                        timescale: scale,
                    },
                    duration: MediaTime {
                        ticks: end - start,
                        timescale: scale,
                    },
                },
                retiming,
                transition_in: None,
                metadata: Metadata::from_tags(src.tags.clone()),
            }));
            n = n.saturating_add(1);
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelforge_capture_core::{
        AudioDevice, CaptureSpec, HZ_1K, MediaRange, MediaTime, SessionMeta,
    };
    use reelforge_capture_schema::CAPTURE_PROJECT_VERSION;
    use reelforge_capture_store::{AudioLegTrack, AudioSegmentFile, AudioSidecar};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn t(s: f64) -> MediaTime {
        MediaTime::from_secs(s, HZ_1K).unwrap()
    }

    fn store_with(
        id: &str,
        segs: &[(u32, f64, f64)],
        audio: bool,
    ) -> (std::path::PathBuf, SessionStore) {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rf-prj-{n}"));
        let mut spec = CaptureSpec::screen();
        if audio {
            spec.audio.system = Some(AudioDevice::named("loop"));
            spec.audio.microphone = Some(AudioDevice::named("mic"));
        }
        let mut store = SessionStore::create(
            &root,
            SessionMeta {
                id: SessionId::new(id),
                name: "demo".into(),
                spec,
                started_unix: None,
                duration: None,
            },
        )
        .unwrap();
        for (ord, a, b) in segs {
            store
                .commit_segment(SegmentRecord {
                    id: SegmentId(*ord),
                    path: format!("segments/{ord:06}.mkv"),
                    start: t(*a),
                    end: t(*b),
                })
                .unwrap();
        }
        (root, store)
    }

    fn kept_all(a: f64, b: f64, speed: f64) -> Vec<KeptRange> {
        vec![KeptRange {
            source: MediaRange::new(t(a), t(b)).unwrap(),
            speed,
        }]
    }

    fn sidecar_for(legs: &[(AudioLeg, u32)], segs: &[(u32, f64, f64)]) -> AudioSidecar {
        let mut s = AudioSidecar::new();
        for (leg, idx) in legs {
            s.legs.push(AudioLegTrack {
                leg: *leg,
                device: "dev".into(),
                audio_index: *idx,
                files: segs
                    .iter()
                    .map(|(ord, a, b)| AudioSegmentFile {
                        segment: SegmentId(*ord),
                        path: format!("audio/{}/{ord:06}.m4a", leg.as_str()),
                        start: t(*a),
                        end: t(*b),
                        duration: Some(t(b - a)),
                        gap: None,
                    })
                    .collect(),
            });
        }
        s
    }

    #[test]
    fn refuses_empty_manifest() {
        let (root, store) = store_with("ses_empty", &[], false);
        let err = project_from_session(&store, &[], &[]).unwrap_err();
        assert!(err.to_string().contains("no committed segments"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn spans_segments_and_emits_audio_tracks() {
        let segs = [(1, 0.0, 5.0), (2, 5.0, 10.0)];
        let (root, store) = store_with("ses_p", &segs, true);
        store
            .write_audio_sidecar(&sidecar_for(
                &[(AudioLeg::System, 0), (AudioLeg::Microphone, 1)],
                &segs,
            ))
            .unwrap();

        let p = project_from_session(&store, &kept_all(3.0, 8.0, 2.0), &[]).unwrap();
        assert_eq!(p.version, CAPTURE_PROJECT_VERSION);
        let seq = p.active().unwrap();
        assert_eq!(seq.tracks.len(), 3);
        assert_eq!(seq.tracks[1].id.as_str(), "a_system");
        assert_eq!(seq.tracks[2].id.as_str(), "a_mic");
        // kept 3–8 crosses both segments → two clips per track
        assert_eq!(seq.tracks[0].items.len(), 2);
        assert_eq!(seq.tracks[1].items.len(), 2);
        assert!(seq.tracks.iter().all(|t| !t.muted));

        // 2 video + 2 system + 2 mic files, each addressable on its own.
        assert_eq!(p.media.len(), 6);
        let audio: Vec<_> = p
            .media
            .iter()
            .filter(|m| m.role.as_deref() == Some("audio"))
            .collect();
        assert_eq!(audio.len(), 4);
        assert!(
            audio.iter().all(|m| std::path::Path::new(&m.uri)
                .extension()
                .is_some_and(|e| e == "m4a")),
            "{audio:?}"
        );

        let text = to_json_pretty(&p).unwrap();
        assert!(text.contains("\"mode\": \"speed\""));
        assert!(text.contains("\"factor\": 2.0"));
        assert!(!text.contains("audio_resolved"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn audio_clips_are_cut_against_their_own_file() {
        let segs = [(1, 0.0, 5.0), (2, 5.0, 10.0)];
        let (root, store) = store_with("ses_off", &segs, true);
        store
            .write_audio_sidecar(&sidecar_for(&[(AudioLeg::System, 0)], &segs))
            .unwrap();
        let p = project_from_session(&store, &kept_all(6.0, 9.0, 1.0), &[]).unwrap();
        let seq = p.active().unwrap();
        let TimelineItem::Clip(clip) = &seq.tracks[1].items[0] else {
            panic!("expected a clip");
        };
        // 6 s on the session clock is 1 s into the second segment's audio file.
        assert!((clip.source.start.as_secs() - 1.0).abs() < 1e-9);
        assert!((clip.source.duration.as_secs() - 3.0).abs() < 1e-9);
        assert_eq!(clip.media.as_str(), "system_000002");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn without_demuxed_files_audio_is_muted_and_flagged() {
        let segs = [(1, 0.0, 5.0)];
        let (root, store) = store_with("ses_raw", &segs, true);
        let p = project_from_session(&store, &kept_all(0.0, 5.0, 1.0), &[]).unwrap();
        let seq = p.active().unwrap();
        assert_eq!(seq.tracks.len(), 3);
        assert!(seq.tracks[1].muted, "unresolved audio must not play");
        let TimelineItem::Clip(clip) = &seq.tracks[2].items[0] else {
            panic!("expected a clip");
        };
        assert_eq!(clip.metadata.tags.get("audio_resolved").unwrap(), "false");
        // system is a:0 → stream 1, microphone is a:1 → stream 2
        assert_eq!(clip.metadata.tags.get("audio_stream").unwrap(), "2");
        assert_eq!(
            unresolved_audio(&store).unwrap(),
            vec![AudioLeg::System, AudioLeg::Microphone]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn mic_only_session_reports_the_first_audio_stream() {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rf-prj-mic-{n}"));
        let mut spec = CaptureSpec::screen();
        spec.audio.microphone = Some(AudioDevice::named("mic"));
        let mut store = SessionStore::create(
            &root,
            SessionMeta {
                id: SessionId::new("ses_mic"),
                name: "demo".into(),
                spec,
                started_unix: None,
                duration: None,
            },
        )
        .unwrap();
        store
            .commit_segment(SegmentRecord {
                id: SegmentId(1),
                path: "segments/000001.mkv".into(),
                start: t(0.0),
                end: t(5.0),
            })
            .unwrap();

        let p = project_from_session(&store, &kept_all(0.0, 5.0, 1.0), &[]).unwrap();
        let seq = p.active().unwrap();
        let TimelineItem::Clip(clip) = &seq.tracks[1].items[0] else {
            panic!("expected a clip");
        };
        assert_eq!(clip.metadata.tags.get("audio_stream").unwrap(), "1");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn click_zooms_stay_markers() {
        let (root, store) = store_with("ses_zoom", &[(1, 0.0, 5.0)], false);
        let zooms = [ClickZoom {
            range: MediaRange::new(t(1.0), t(1.4)).unwrap(),
            x: 100,
            y: 80,
            scale: 1.8,
        }];
        let p = project_from_session(&store, &kept_all(0.0, 5.0, 1.0), &zooms).unwrap();
        let seq = p.active().unwrap();
        assert_eq!(seq.markers.len(), 1);
        assert_eq!(seq.markers[0].name, "click_zoom_0");
        assert_eq!(seq.markers[0].semantic.as_ref().unwrap().id, "click:100,80");
        assert_eq!(seq.tracks.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }
}
