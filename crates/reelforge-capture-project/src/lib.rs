//! Headless `CaptureProject` v1 authoring.
//!
//! The document shape lives in [`reelforge_capture_schema`] — the typed
//! contract shared with `ReelForge`. This crate only decides *what* to put in
//! it: which files become media, which kept ranges become clips, and how
//! audio legs are addressed.
//!
//! # Host ingest
//!
//! Capture stays grab + project. Host does **not** walk `sessions/<id>/` —
//! an uncommitted tail and leftover files live there. The ingest list is
//! [`ingest_video_media`]: committed segments only, each with an absolute
//! filesystem URI and a duration. Those URIs are what Host passes as
//! `--video` / `ingest_video`. [`project_from_session`] writes the same
//! entries into `CaptureProject.media`.
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
use reelforge_capture_edit::{ClickZoom, KeptRange, ZoomSlice, zoom_slices};
use reelforge_capture_platform::probe_video_size;
use reelforge_capture_schema::{
    CaptureProject, CropRect, Gap, Marker, MediaRef, MediaRefId, Metadata, ProjectId, Retiming,
    SemanticRef, Sequence, SequenceId, SourceRange, TimelineClip, TimelineClipId, TimelineItem,
    TimelineTrack, TimelineTrackId, TrackKind,
};
use reelforge_capture_store::{AudioSidecar, ClockSidecar, SegmentRecord, SessionStore};

/// One file a clip can be cut from: a media entry plus its session span.
struct Source {
    media: MediaRefId,
    /// Start on the session clock (alignment slot).
    start: i64,
    /// End of the alignment slot (video segment / session span).
    end: i64,
    /// Where the file actually stops on the session clock (`≤ end`).
    ///
    /// Short audio or a dead stretch after a restart leaves `[playable_end, end)`
    /// as a timeline [`Gap`] so later clips stay lined up with video.
    playable_end: i64,
    /// Session time of file t = 0.
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
    project_from_session_sized(store, kept, zooms, None)
}

/// [`project_from_session`] with an explicit frame size (skips ffprobe).
///
/// # Errors
///
/// No committed segments, an inconsistent document, or JSON.
pub fn project_from_session_sized(
    store: &SessionStore,
    kept: &[KeptRange],
    zooms: &[ClickZoom],
    frame: Option<(u32, u32)>,
) -> Result<CaptureProject> {
    let meta = &store.manifest().meta;
    let segments = &store.manifest().segments;
    if segments.is_empty() {
        return Err(CaptureError::message(
            "no committed segments; cannot emit a project (run a supervised capture first)",
        ));
    }
    let sidecar = store.read_audio_sidecar()?;
    let clocks = store.read_clocks()?.unwrap_or_default();
    let has_waveform = store.read_waveforms()?.is_some();
    let frame = frame.or_else(|| {
        segments
            .first()
            .and_then(|s| probe_video_size(store.root().join(&s.path)).ok().flatten())
    });
    let slices = match frame {
        Some((w, h)) => zoom_slices(zooms, w, h)?,
        None => Vec::new(),
    };

    let mut project = CaptureProject::new(ProjectId::new(meta.id.as_str()), meta.name.clone());
    let mut meta_tags: Vec<(String, String)> = vec![
        ("producer".into(), "reelforge-capture".into()),
        ("session_id".into(), meta.id.as_str().into()),
    ];
    if !clocks.segments.is_empty() {
        meta_tags.push(("clocks".into(), "clocks.json".into()));
    }
    project.metadata = Metadata::from_tags(meta_tags);

    let video_sources: Vec<Source> = segments
        .iter()
        .map(|s| Source {
            media: video_media_id(s),
            start: s.start.ticks,
            end: s.end.ticks,
            playable_end: s.end.ticks,
            file_base: s.start.ticks,
            tags: clock_video_tags(&clocks, s),
        })
        .collect();
    // Same list Host reads for `--video` / `ingest_video`. Do not glob the
    // session directory: only committed segments belong here.
    project.media = ingest_video_media(store)?;

    let mut tracks = vec![TimelineTrack {
        id: TimelineTrackId::new("v0"),
        kind: TrackKind::Video,
        items: clips_for_ranges(&video_sources, kept, "c", &slices),
        muted: false,
    }];

    for (leg, _device) in meta.spec.audio.configured() {
        let plan = plan_audio_leg(
            store,
            segments,
            sidecar.as_ref(),
            &clocks,
            leg,
            has_waveform,
        );
        project.media.extend(plan.media);
        let mut track = TimelineTrack::new(audio_track_id(leg), TrackKind::Audio);
        track.items = clips_for_ranges(&plan.sources, kept, audio_clip_prefix(leg), &[]);
        track.muted = plan.muted;
        tracks.push(track);
    }

    let mut main = Sequence::new(SequenceId::new("main"), "main");
    main.canvas = frame;
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
    clocks: &ClockSidecar,
    leg: AudioLeg,
    has_waveform: bool,
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
            if let Some(start_ms) = clock_audio_start_ms(clocks, f.segment, store, leg) {
                tags.push(("audio_start_ms".into(), start_ms.to_string()));
            }
            if has_waveform {
                tags.push(("waveform".into(), "waveforms.json".into()));
                tags.push(("waveform_leg".into(), leg.as_str().into()));
            }
            let duration = f
                .duration
                .or_else(|| clock_audio_duration(clocks, f.segment, store, leg, f.start.timescale));
            plan.media.push(MediaRef {
                id: id.clone(),
                uri: media_uri(store, &f.path),
                duration: Some(file_duration(f.start, f.end, duration)),
                role: Some("audio".into()),
            });
            // Extra audio past the video slot is trimmed. A shortfall
            // (`playable_end < end`) becomes a timeline gap.
            let playable = duration
                .map_or(f.end.ticks, |d| {
                    f.start.ticks.saturating_add(d.ticks.max(0))
                })
                .clamp(f.start.ticks, f.end.ticks);
            plan.sources.push(Source {
                media: id,
                start: f.start.ticks,
                end: f.end.ticks,
                playable_end: playable,
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
                playable_end: s.end.ticks,
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

/// Committed video files Host may pass as `--video` / `ingest_video`.
///
/// One entry per **committed** segment, in record order, each with an
/// absolute filesystem URI and a duration on the session clock. Loose files
/// under `segments/` that the WAL never committed are not listed — Host
/// must not glob the session directory.
///
/// # Errors
///
/// No committed segments.
pub fn ingest_video_media(store: &SessionStore) -> Result<Vec<MediaRef>> {
    let segments = &store.manifest().segments;
    if segments.is_empty() {
        return Err(CaptureError::message(
            "no committed segments; cannot emit media (run a supervised capture first)",
        ));
    }
    Ok(segments.iter().map(|s| video_media_ref(store, s)).collect())
}

/// Demuxed audio files from `audio.json`, when present.
///
/// Same URI + duration contract as [`ingest_video_media`]. Empty when the
/// session has no sidecar (or no extracted files) — those legs stay muted
/// in the project instead of pointing at a muxed stream.
///
/// # Errors
///
/// Reading `audio.json`.
pub fn ingest_audio_media(store: &SessionStore) -> Result<Vec<MediaRef>> {
    let Some(sidecar) = store.read_audio_sidecar()? else {
        return Ok(Vec::new());
    };
    Ok(sidecar
        .legs
        .iter()
        .flat_map(|track| {
            track.files.iter().map(|f| MediaRef {
                id: audio_media_id(track.leg, f.segment),
                uri: media_uri(store, &f.path),
                duration: Some(file_duration(f.start, f.end, f.duration)),
                role: Some("audio".into()),
            })
        })
        .collect())
}

/// Absolute filesystem URI Host can hand to ffmpeg / `ingest_video`.
///
/// Not `file://` and not a `\\?\` canonical path — both break Host / ffmpeg
/// `--video`. Relative session roots are resolved against the process cwd.
#[must_use]
pub fn media_uri(store: &SessionStore, rel: &str) -> String {
    let path = store.root().join(rel);
    let abs = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    };
    abs.to_string_lossy().replace('\\', "/")
}

fn video_media_ref(store: &SessionStore, seg: &SegmentRecord) -> MediaRef {
    MediaRef {
        id: video_media_id(seg),
        uri: media_uri(store, &seg.path),
        duration: Some(seg_duration(seg)),
        role: Some("video".into()),
    }
}

fn clock_video_tags(clocks: &ClockSidecar, seg: &SegmentRecord) -> Vec<(String, String)> {
    let Some(row) = clocks.segment(seg.id) else {
        return Vec::new();
    };
    let mut tags = vec![
        ("clock_master".into(), row.master.as_str().into()),
        ("clock_correction_ms".into(), row.correction_ms.to_string()),
    ];
    if let Some(v) = row.video_secs {
        tags.push(("clock_video_secs".into(), format!("{v:.3}")));
    }
    tags
}

fn clock_audio_index(store: &SessionStore, leg: AudioLeg) -> Option<u32> {
    store.manifest().meta.spec.audio.audio_index(leg)
}

fn clock_audio_duration(
    clocks: &ClockSidecar,
    segment: SegmentId,
    store: &SessionStore,
    leg: AudioLeg,
    scale: u32,
) -> Option<MediaTime> {
    let index = clock_audio_index(store, leg)?;
    let secs = clocks.segment(segment)?.audio_leg(index)?.duration_secs?;
    MediaTime::from_secs(secs, scale.max(1)).ok()
}

fn clock_audio_start_ms(
    clocks: &ClockSidecar,
    segment: SegmentId,
    store: &SessionStore,
    leg: AudioLeg,
) -> Option<i64> {
    let index = clock_audio_index(store, leg)?;
    let secs = clocks.segment(segment)?.audio_leg(index)?.start_secs?;
    if !(secs > 0.0 && secs <= 0.25) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    Some((secs * 1_000.0).round() as i64)
}

fn file_duration(start: MediaTime, end: MediaTime, measured: Option<MediaTime>) -> MediaTime {
    measured.unwrap_or_else(|| MediaTime {
        ticks: (end.ticks - start.ticks).max(0),
        timescale: start.timescale.max(1),
    })
}

/// Pretty JSON for `CaptureProject::from_json`.
///
/// # Errors
///
/// Serde.
pub fn to_json_pretty(project: &CaptureProject) -> Result<String> {
    project.to_json_pretty()
}

/// Read a project document back (`version: 0` is migrated).
///
/// # Errors
///
/// JSON, or a version newer than this schema.
pub fn parse_project(text: &str) -> Result<CaptureProject> {
    CaptureProject::from_json(text)
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

fn seg_duration(seg: &SegmentRecord) -> MediaTime {
    MediaTime {
        ticks: (seg.end.ticks - seg.start.ticks).max(0),
        timescale: seg.start.timescale.max(1),
    }
}

/// Cut each kept range against each source file, in record order.
///
/// Uncovered session time — a short audio file, a missing demux, a restart
/// hole — becomes a [`Gap`] so the next clip stays aligned with video.
fn clips_for_ranges(
    sources: &[Source],
    kept: &[KeptRange],
    prefix: &str,
    zooms: &[ZoomSlice],
) -> Vec<TimelineItem> {
    let mut items = Vec::new();
    let mut n = 0u32;
    for k in kept {
        let scale = k.source.start.timescale.max(1);
        let mut cursor = k.source.start.ticks;
        let end = k.source.end.ticks;
        while cursor < end {
            let Some(src) = sources.iter().find(|s| s.start <= cursor && cursor < s.end) else {
                let next = sources
                    .iter()
                    .filter(|s| s.start > cursor)
                    .map(|s| s.start)
                    .min()
                    .unwrap_or(end)
                    .min(end);
                push_gap(&mut items, next - cursor, scale, k.speed);
                cursor = next;
                continue;
            };
            let playable = src.playable_end.min(src.end).min(end);
            if cursor < playable {
                n += emit_video_span(
                    &mut items, src, cursor, playable, scale, k.speed, prefix, n, zooms,
                );
                cursor = playable;
                continue;
            }
            let gap_until = src.end.min(end);
            if gap_until > cursor {
                push_gap(&mut items, gap_until - cursor, scale, k.speed);
                cursor = gap_until;
            } else {
                break;
            }
        }
    }
    items
}

#[allow(clippy::too_many_arguments)]
fn emit_video_span(
    items: &mut Vec<TimelineItem>,
    src: &Source,
    start: i64,
    end: i64,
    scale: u32,
    speed: f64,
    prefix: &str,
    mut n: u32,
    zooms: &[ZoomSlice],
) -> u32 {
    let retiming = if (speed - 1.0).abs() < 1e-9 {
        Retiming::Identity
    } else {
        Retiming::Speed { factor: speed }
    };
    let mut cursor = start;
    while cursor < end {
        let covering = zooms
            .iter()
            .find(|z| z.range.start.ticks <= cursor && cursor < z.range.end.ticks);
        let until = if let Some(z) = covering {
            z.range.end.ticks.min(end)
        } else {
            zooms
                .iter()
                .filter(|z| z.range.start.ticks > cursor)
                .map(|z| z.range.start.ticks)
                .min()
                .unwrap_or(end)
                .min(end)
        };
        if until <= cursor {
            break;
        }
        let (crop, scale_to) = covering
            .filter(|z| z.is_zoomed())
            .map_or((None, None), |z| {
                (
                    Some(CropRect {
                        x: z.crop.x,
                        y: z.crop.y,
                        w: z.crop.w,
                        h: z.crop.h,
                    }),
                    Some(z.scale_to),
                )
            });
        items.push(TimelineItem::Clip(TimelineClip {
            id: TimelineClipId::new(format!("{prefix}{n}")),
            media: src.media.clone(),
            source: SourceRange {
                start: MediaTime {
                    ticks: cursor - src.file_base,
                    timescale: scale,
                },
                duration: MediaTime {
                    ticks: until - cursor,
                    timescale: scale,
                },
            },
            retiming: retiming.clone(),
            transition_in: None,
            crop,
            scale_to,
            metadata: Metadata::from_tags(src.tags.clone()),
        }));
        n = n.saturating_add(1);
        cursor = until;
    }
    n
}

fn push_gap(items: &mut Vec<TimelineItem>, source_ticks: i64, scale: u32, speed: f64) {
    let ticks = record_ticks(source_ticks, speed);
    if ticks <= 0 {
        return;
    }
    let add = MediaTime {
        ticks,
        timescale: scale,
    };
    if let Some(TimelineItem::Gap(gap)) = items.last_mut() {
        gap.duration.ticks = gap.duration.ticks.saturating_add(add.ticks);
        return;
    }
    items.push(TimelineItem::Gap(Gap { duration: add }));
}

fn record_ticks(source_ticks: i64, speed: f64) -> i64 {
    if source_ticks <= 0 {
        return 0;
    }
    if !(speed.is_finite() && speed > 0.0) || (speed - 1.0).abs() < 1e-9 {
        return source_ticks;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    {
        (source_ticks as f64 / speed).round() as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelforge_capture_core::{
        AudioDevice, CaptureSpec, HZ_1K, MediaRange, MediaTime, SessionMeta,
    };
    use reelforge_capture_schema::CAPTURE_PROJECT_VERSION;
    use reelforge_capture_store::{
        AudioLegTrack, AudioSegmentFile, AudioSidecar, ClockAudioLeg, ClockMaster, ClockSegment,
    };
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
        assert!(!text.contains("waveform"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ingest_video_lists_committed_segments_only() {
        let segs = [(1, 0.0, 5.0), (2, 5.0, 10.0)];
        let (root, store) = store_with("ses_host", &segs, false);
        // A leftover file Host would pick up if it globbed sessions/<id>/.
        std::fs::write(store.root().join("segments/000003.mkv"), b"not committed").unwrap();

        let listed = ingest_video_media(&store).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|m| m.duration.is_some()));
        assert!(listed.iter().all(|m| m.role.as_deref() == Some("video")));
        assert!(listed.iter().all(|m| {
            let p = std::path::Path::new(&m.uri);
            p.is_absolute() && p.extension().is_some_and(|e| e == "mkv")
        }));
        assert!(!listed.iter().any(|m| m.uri.contains("000003")));

        let p = project_from_session(&store, &kept_all(0.0, 10.0, 1.0), &[]).unwrap();
        let video: Vec<_> = p
            .media
            .iter()
            .filter(|m| m.role.as_deref() == Some("video"))
            .cloned()
            .collect();
        assert_eq!(video, listed);
        assert_eq!(
            p.metadata.tags.get("producer").map(String::as_str),
            Some("reelforge-capture")
        );
        assert_eq!(
            p.metadata.tags.get("session_id").map(String::as_str),
            Some("ses_host")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_tags_clocks_from_the_sidecar() {
        let segs = [(1, 0.0, 5.0)];
        let (root, store) = store_with("ses_clk", &segs, true);
        store
            .write_audio_sidecar(&sidecar_for(&[(AudioLeg::System, 0)], &segs))
            .unwrap();
        store
            .append_clock(ClockSegment {
                id: SegmentId(1),
                start: t(0.0),
                end: t(5.0),
                master: ClockMaster::Video,
                session_secs: 5.2,
                video_secs: Some(5.0),
                audio_secs: Some(4.94),
                video_start_secs: None,
                audio: vec![ClockAudioLeg {
                    index: 0,
                    duration_secs: Some(4.94),
                    start_secs: Some(0.021),
                }],
                correction_ms: -200,
            })
            .unwrap();

        let p = project_from_session(&store, &kept_all(0.0, 5.0, 1.0), &[]).unwrap();
        assert_eq!(
            p.metadata.tags.get("clocks").map(String::as_str),
            Some("clocks.json")
        );
        let TimelineItem::Clip(video) = &p.active().unwrap().tracks[0].items[0] else {
            panic!("expected video clip");
        };
        assert_eq!(video.metadata.tags.get("clock_master").unwrap(), "video");
        assert_eq!(
            video.metadata.tags.get("clock_correction_ms").unwrap(),
            "-200"
        );
        let TimelineItem::Clip(audio) = &p.active().unwrap().tracks[1].items[0] else {
            panic!("expected audio clip");
        };
        assert_eq!(audio.metadata.tags.get("audio_start_ms").unwrap(), "21");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ingest_audio_fills_duration_when_sidecar_omits_it() {
        let segs = [(1, 0.0, 5.0)];
        let (root, store) = store_with("ses_adur", &segs, true);
        let mut side = sidecar_for(&[(AudioLeg::System, 0)], &segs);
        side.legs[0].files[0].duration = None;
        store.write_audio_sidecar(&side).unwrap();

        let audio = ingest_audio_media(&store).unwrap();
        assert_eq!(audio.len(), 1);
        let d = audio[0].duration.expect("duration is required for Host");
        assert!((d.as_secs() - 5.0).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_tags_audio_when_waveforms_exist() {
        use reelforge_capture_store::{WaveformLeg, WaveformPeak, WaveformSidecar};
        let segs = [(1, 0.0, 5.0)];
        let (root, store) = store_with("ses_wf", &segs, true);
        store
            .write_audio_sidecar(&sidecar_for(&[(AudioLeg::System, 0)], &segs))
            .unwrap();
        let mut wf = WaveformSidecar::new(8_000, 0.05);
        wf.legs.push(WaveformLeg {
            leg: AudioLeg::System,
            peaks: vec![WaveformPeak {
                t0: t(0.0),
                t1: t(0.05),
                min: -0.1,
                max: 0.2,
            }],
        });
        store.write_waveforms(&wf).unwrap();
        let p = project_from_session(&store, &kept_all(0.0, 5.0, 1.0), &[]).unwrap();
        let TimelineItem::Clip(clip) = &p.active().unwrap().tracks[1].items[0] else {
            panic!("expected clip");
        };
        assert_eq!(
            clip.metadata.tags.get("waveform").unwrap(),
            "waveforms.json"
        );
        assert_eq!(clip.metadata.tags.get("waveform_leg").unwrap(), "system");
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
    fn short_audio_becomes_a_timeline_gap() {
        let segs = [(1, 0.0, 5.0)];
        let (root, store) = store_with("ses_gap", &segs, true);
        let mut side = sidecar_for(&[(AudioLeg::System, 0)], &segs);
        side.legs[0].files[0].duration = Some(t(4.94));
        side.legs[0].files[0].gap = Some(MediaTime {
            ticks: -60,
            timescale: HZ_1K,
        });
        store.write_audio_sidecar(&side).unwrap();

        let p = project_from_session(&store, &kept_all(0.0, 5.0, 1.0), &[]).unwrap();
        let audio = &p.active().unwrap().tracks[1].items;
        assert_eq!(audio.len(), 2, "{audio:?}");
        let TimelineItem::Clip(clip) = &audio[0] else {
            panic!("expected clip first: {audio:?}");
        };
        assert!((clip.source.duration.as_secs() - 4.94).abs() < 1e-9);
        let TimelineItem::Gap(gap) = &audio[1] else {
            panic!("expected gap after short audio: {audio:?}");
        };
        assert!((gap.duration.as_secs() - 0.06).abs() < 1e-9);
        assert_eq!(
            clip.metadata.tags.get("audio_gap_ms").map(String::as_str),
            Some("-60")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn restart_hole_on_video_is_a_gap() {
        let segs = [(1, 0.0, 5.0), (2, 7.0, 12.0)];
        let (root, store) = store_with("ses_hole", &segs, false);
        let p = project_from_session(&store, &kept_all(0.0, 12.0, 1.0), &[]).unwrap();
        let video = &p.active().unwrap().tracks[0].items;
        assert_eq!(video.len(), 3, "{video:?}");
        assert!(matches!(video[0], TimelineItem::Clip(_)));
        let TimelineItem::Gap(gap) = &video[1] else {
            panic!("expected hole gap: {video:?}");
        };
        assert!((gap.duration.as_secs() - 2.0).abs() < 1e-9);
        assert!(matches!(video[2], TimelineItem::Clip(_)));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sped_gap_shrinks_with_the_keep() {
        let segs = [(1, 0.0, 5.0)];
        let (root, store) = store_with("ses_sgap", &segs, true);
        let mut side = sidecar_for(&[(AudioLeg::System, 0)], &segs);
        side.legs[0].files[0].duration = Some(t(4.0));
        side.legs[0].files[0].gap = Some(MediaTime {
            ticks: -1000,
            timescale: HZ_1K,
        });
        store.write_audio_sidecar(&side).unwrap();
        let p = project_from_session(&store, &kept_all(0.0, 5.0, 2.0), &[]).unwrap();
        let audio = &p.active().unwrap().tracks[1].items;
        let TimelineItem::Gap(gap) = &audio[1] else {
            panic!("expected sped gap: {audio:?}");
        };
        // 1 s of missing source at 2× → 0.5 s on the record.
        assert!((gap.duration.as_secs() - 0.5).abs() < 1e-9);
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

    #[test]
    fn click_zooms_become_cropped_clips() {
        let (root, store) = store_with("ses_crop", &[(1, 0.0, 5.0)], false);
        let zooms = [ClickZoom {
            range: MediaRange::new(t(1.0), t(2.0)).unwrap(),
            x: 160,
            y: 90,
            scale: 2.0,
        }];
        let p =
            project_from_session_sized(&store, &kept_all(0.0, 5.0, 1.0), &zooms, Some((320, 180)))
                .unwrap();
        let seq = p.active().unwrap();
        assert_eq!(seq.canvas, Some((320, 180)));
        let clips: Vec<_> = seq.tracks[0]
            .items
            .iter()
            .filter_map(|i| match i {
                TimelineItem::Clip(c) => Some(c),
                _ => None,
            })
            .collect();
        assert!(clips.len() >= 3, "{} clips: {clips:?}", clips.len());
        let zoomed: Vec<_> = clips.iter().filter(|c| c.crop.is_some()).collect();
        assert!(!zoomed.is_empty());
        assert!(zoomed.iter().all(|c| c.scale_to == Some((320, 180))));
        assert!(
            zoomed
                .iter()
                .any(|c| c.crop.is_some_and(|b| b.w == 160 && b.h == 90))
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
