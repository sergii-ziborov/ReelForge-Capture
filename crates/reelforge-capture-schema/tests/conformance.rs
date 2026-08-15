//! Cross-repo conformance: the golden document is the artifact `ReelForge`
//! parses. Re-bless it with `REELFORGE_BLESS=1 cargo test -p reelforge-capture-schema`.

use reelforge_capture_core::{HZ_1K, MediaTime};
use reelforge_capture_schema::{
    CAPTURE_PROJECT_VERSION, CaptureProject, Gap, Marker, MediaRef, MediaRefId, Metadata,
    NestedSequence, ProjectId, Retiming, SemanticRef, Sequence, SequenceId, SourceRange,
    TimelineClip, TimelineClipId, TimelineItem, TimelineTrack, TimelineTrackId, TrackKind,
    Transition, TransitionKind,
};
use std::path::PathBuf;

fn t(secs: f64) -> MediaTime {
    MediaTime::from_secs(secs, HZ_1K).expect("media time")
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/capture_project_v1.json")
}

/// Every field and every enum variant of the v1 contract, in one document.
fn golden_project() -> CaptureProject {
    let mut project = CaptureProject::new(ProjectId::new("ses_golden"), "golden");
    project.metadata = Metadata::from_tags([("producer", "reelforge-capture")]);
    project.semantic = vec![SemanticRef::new("query", "q_1")];
    project.markers = vec![Marker {
        t: t(0.0),
        duration: None,
        name: "project_marker".into(),
        semantic: None,
    }];
    project.media = vec![
        MediaRef {
            id: MediaRefId::new("seg_000001"),
            uri: "sessions/ses_golden/segments/000001.mkv".into(),
            duration: Some(t(5.0)),
            role: Some("video".into()),
        },
        MediaRef {
            id: MediaRefId::new("system_000001"),
            uri: "sessions/ses_golden/audio/system/000001.m4a".into(),
            duration: Some(t(5.0)),
            role: Some("audio".into()),
        },
    ];

    let mut video = TimelineTrack::new(TimelineTrackId::new("v0"), TrackKind::Video);
    video.items = vec![
        TimelineItem::Clip(TimelineClip {
            id: TimelineClipId::new("c0"),
            media: MediaRefId::new("seg_000001"),
            source: SourceRange {
                start: t(0.0),
                duration: t(2.0),
            },
            retiming: Retiming::Identity,
            transition_in: None,
            metadata: Metadata::default(),
        }),
        TimelineItem::Clip(TimelineClip {
            id: TimelineClipId::new("c1"),
            media: MediaRefId::new("seg_000001"),
            source: SourceRange {
                start: t(2.0),
                duration: t(3.0),
            },
            retiming: Retiming::Speed { factor: 2.0 },
            transition_in: Some(Transition {
                kind: TransitionKind::Dissolve,
                duration: t(0.25),
            }),
            metadata: Metadata::from_tags([("origin", "speed_op")]),
        }),
        TimelineItem::Gap(Gap { duration: t(0.5) }),
        TimelineItem::Nested(NestedSequence {
            sequence: SequenceId::new("insert"),
            duration: Some(t(1.0)),
        }),
    ];

    let mut audio = TimelineTrack::new(TimelineTrackId::new("a_system"), TrackKind::Audio);
    audio.items = vec![TimelineItem::Clip(TimelineClip {
        id: TimelineClipId::new("as0"),
        media: MediaRefId::new("system_000001"),
        source: SourceRange {
            start: t(0.0),
            duration: t(5.0),
        },
        retiming: Retiming::Identity,
        transition_in: None,
        metadata: Metadata::from_tags([("audio_leg", "system"), ("gap_ms", "12")]),
    })];

    let mut main = Sequence::new(SequenceId::new("main"), "main");
    main.canvas = Some((1920, 1080));
    main.tracks = vec![video, audio];
    main.markers = vec![Marker {
        t: t(1.0),
        duration: Some(t(0.4)),
        name: "click_zoom_0".into(),
        semantic: Some(SemanticRef::new("event", "click:100,80")),
    }];

    let mut insert = Sequence::new(SequenceId::new("insert"), "insert");
    insert.tracks = vec![TimelineTrack {
        id: TimelineTrackId::new("v0"),
        kind: TrackKind::Video,
        items: Vec::new(),
        muted: true,
    }];

    project.active_sequence = Some(SequenceId::new("main"));
    project.sequences = vec![main, insert];
    project
}

#[test]
fn golden_document_matches_the_typed_schema() {
    let built = golden_project();
    built
        .validate()
        .expect("golden document is self-consistent");
    let text = format!("{}\n", built.to_json_pretty().expect("serialize"));
    let path = golden_path();

    if std::env::var("REELFORGE_BLESS").is_ok() {
        std::fs::create_dir_all(path.parent().expect("golden dir")).expect("mkdir");
        std::fs::write(&path, &text).expect("bless golden");
    }

    let on_disk = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "read {}: {e} (bless with REELFORGE_BLESS=1)",
            path.display()
        )
    });
    assert_eq!(
        on_disk.replace("\r\n", "\n"),
        text,
        "CaptureProject v1 wire format changed; re-bless and sync ReelForge"
    );
}

#[test]
fn golden_document_parses_back_into_the_same_value() {
    let text = std::fs::read_to_string(golden_path()).expect("golden document");
    let parsed = CaptureProject::from_json(&text).expect("parse golden");
    assert_eq!(parsed.version, CAPTURE_PROJECT_VERSION);
    assert_eq!(parsed, golden_project());
    parsed.validate().expect("valid");
    assert_eq!(parsed.active().expect("active").id.as_str(), "main");
}
