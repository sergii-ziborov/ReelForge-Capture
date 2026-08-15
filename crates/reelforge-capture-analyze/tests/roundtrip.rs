//! End-to-end check against a **real host ffmpeg**, on synthetic segments
//! (no screen is recorded).
//!
//! Ignored by default because CI runners do not all ship ffmpeg:
//!
//! ```text
//! cargo test -p reelforge-capture-analyze -- --ignored
//! ```
//!
//! Segment 1 is busy (moving picture, two tones). Segment 2 is dead (black
//! frame, digital silence). The session therefore has a known answer: idle
//! is the second half, and it must be found without a single pointer event.

use reelforge_capture_analyze::{SignalOptions, materialize_audio, session_signals};
use reelforge_capture_core::{
    AudioDevice, AudioLeg, CaptureSpec, HZ_1K, MediaTime, SessionId, SessionMeta, SignalKind,
};
use reelforge_capture_edit::{IdleConfig, KeptRange, detect_idle_multi};
use reelforge_capture_project::project_from_session;
use reelforge_capture_store::{SegmentRecord, SessionStore};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn t(secs: f64) -> MediaTime {
    MediaTime::from_secs(secs, HZ_1K).expect("media time")
}

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn ffmpeg(args: &[&str], out: &Path) {
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-y"])
        .args(args)
        .arg(out)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed for {}", out.display());
}

/// Busy segment: moving picture + two distinct tones.
#[rustfmt::skip]
fn write_busy(out: &Path) {
    ffmpeg(
        &[
            "-f", "lavfi", "-i", "testsrc=size=320x240:rate=10:duration=5",
            "-f", "lavfi", "-i", "sine=frequency=440:duration=5",
            "-f", "lavfi", "-i", "sine=frequency=880:duration=5",
            "-map", "0:v", "-map", "1:a", "-map", "2:a",
            "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
            "-c:a", "aac",
        ],
        out,
    );
}

/// Dead segment: black frame + digital silence on both legs.
#[rustfmt::skip]
fn write_idle(out: &Path) {
    ffmpeg(
        &[
            "-f", "lavfi", "-i", "color=c=black:s=320x240:r=10:d=5",
            "-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo:d=5",
            "-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo:d=5",
            "-map", "0:v", "-map", "1:a", "-map", "2:a",
            "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
            "-c:a", "aac",
        ],
        out,
    );
}

fn session() -> (PathBuf, SessionStore) {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("rf-analyze-{n}"));
    let mut spec = CaptureSpec::screen();
    spec.audio.system = Some(AudioDevice::named("loop"));
    spec.audio.microphone = Some(AudioDevice::named("mic"));
    let mut store = SessionStore::create(
        &root,
        SessionMeta {
            id: SessionId::new("ses_e2e"),
            name: "e2e".into(),
            spec,
            started_unix: None,
            duration: None,
        },
    )
    .expect("create session");

    write_busy(&store.root().join("segments/000001.mkv"));
    write_idle(&store.root().join("segments/000002.mkv"));
    for (ord, a, b) in [(1u32, 0.0, 5.0), (2, 5.0, 10.0)] {
        store
            .commit_segment(SegmentRecord {
                id: reelforge_capture_core::SegmentId(ord),
                path: format!("segments/{ord:06}.mkv"),
                start: t(a),
                end: t(b),
            })
            .expect("commit");
    }
    (root, store)
}

#[test]
#[ignore = "needs host ffmpeg"]
fn demuxed_audio_and_measured_signals_find_idle_without_a_pointer_log() {
    if !have_ffmpeg() {
        eprintln!("skipping: no host ffmpeg");
        return;
    }
    let (root, store) = session();

    // 1. Each leg becomes its own single-stream file.
    let done = materialize_audio(&store).expect("materialize");
    assert!(done.failures.is_empty(), "{:?}", done.failures);
    assert_eq!(done.extracted, 4, "2 legs × 2 segments");
    assert_eq!(done.sidecar.legs.len(), 2);
    for leg in [AudioLeg::System, AudioLeg::Microphone] {
        let track = done.sidecar.leg(leg).expect("leg");
        assert_eq!(track.files.len(), 2);
        for f in &track.files {
            let path = store.root().join(&f.path);
            assert!(path.is_file(), "missing {}", path.display());
            assert!(f.duration.is_some(), "unprobed {}", f.path);
        }
    }
    // Extraction is idempotent.
    let again = materialize_audio(&store).expect("materialize twice");
    assert_eq!((again.extracted, again.reused), (0, 4));

    // 2. Both signals are measured across the whole session.
    let tracks = session_signals(&store, &SignalOptions::default()).expect("signals");
    assert_eq!(tracks.len(), 3, "motion + 2 audio legs: {tracks:?}");
    let motion = tracks
        .iter()
        .find(|t| t.kind == SignalKind::Motion)
        .expect("motion track");
    let busy = motion
        .samples
        .iter()
        .filter(|s| s.t.as_secs() < 4.5 && s.value > 1.0)
        .count();
    let dead = motion
        .samples
        .iter()
        .filter(|s| s.t.as_secs() > 5.5 && s.value > 1.0)
        .count();
    assert!(busy > 4, "moving picture should register: {busy}");
    assert_eq!(dead, 0, "black frames must not register as motion");

    for leg in ["audio:system", "audio:microphone"] {
        let audio = tracks.iter().find(|t| t.source == leg).expect(leg);
        assert!(
            audio
                .samples
                .iter()
                .any(|s| s.t.as_secs() < 4.5 && s.value > -45.0),
            "{leg} should hear the tone"
        );
        assert!(
            audio
                .samples
                .iter()
                .filter(|s| s.t.as_secs() > 5.5)
                .all(|s| s.value < -45.0),
            "{leg} should be silent in the dead segment"
        );
    }

    // 3. Idle is found from measured signals alone — no pointer events at all.
    let report = detect_idle_multi(&[], &tracks, t(10.0), IdleConfig::new(t(2.0))).expect("idle");
    assert!(!report.is_blind(), "signals must vote");
    assert_eq!(report.sources.len(), 3);
    let idle = report
        .ranges
        .iter()
        .find(|r| r.duration_secs() > 2.0)
        .unwrap_or_else(|| panic!("expected an idle stretch: {:?}", report.ranges));
    assert!(
        idle.start.as_secs() >= 4.0 && idle.end.as_secs() <= 10.0,
        "idle should be the dead second half: {idle:?}"
    );

    // 4. The project addresses audio by file, not by stream index.
    let kept = [KeptRange {
        source: reelforge_capture_core::MediaRange::new(t(0.0), t(10.0)).expect("range"),
        speed: 1.0,
    }];
    let project = project_from_session(&store, &kept, &[]).expect("project");
    project.validate().expect("valid project");
    let seq = project.active().expect("sequence");
    assert_eq!(seq.tracks.len(), 3);
    assert!(
        seq.tracks.iter().all(|t| !t.muted),
        "demuxed audio must not be muted"
    );
    let audio_media: Vec<_> = project
        .media
        .iter()
        .filter(|m| m.role.as_deref() == Some("audio"))
        .collect();
    assert_eq!(audio_media.len(), 4);
    for m in audio_media {
        assert!(Path::new(&m.uri).is_file(), "dangling uri {}", m.uri);
    }

    let _ = std::fs::remove_dir_all(root);
}
