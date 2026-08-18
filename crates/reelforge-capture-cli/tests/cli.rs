//! Drive the binary against a synthetic session (no screen is recorded).
//!
//! Ignored by default — needs host ffmpeg:
//!
//! ```text
//! cargo test -p reelforge-capture-cli -- --ignored
//! ```

use reelforge_capture_core::{
    AudioDevice, CaptureSpec, HZ_1K, MediaTime, SegmentId, SessionId, SessionMeta,
};
use reelforge_capture_store::{SegmentRecord, SessionStore};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const BIN: &str = env!("CARGO_BIN_EXE_reelforge-capture");

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[rustfmt::skip]
fn segment(out: &Path, video: &str, audio: &str) {
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-y"])
        .args(["-f", "lavfi", "-i", video])
        .args(["-f", "lavfi", "-i", audio])
        .args(["-f", "lavfi", "-i", audio])
        .args(["-map", "0:v", "-map", "1:a", "-map", "2:a"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"])
        .args(["-c:a", "aac"])
        .arg(out)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed for {}", out.display());
}

/// Busy first half, dead second half — the same fixture the analyze e2e uses.
fn synthetic_session() -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("rf-cli-{n}"));
    let mut spec = CaptureSpec::screen();
    spec.audio.system = Some(AudioDevice::named("loop"));
    spec.audio.microphone = Some(AudioDevice::named("mic"));
    let mut store = SessionStore::create(
        &root,
        SessionMeta {
            id: SessionId::new("ses_cli"),
            name: "cli".into(),
            spec,
            started_unix: None,
            duration: None,
        },
    )
    .expect("create session");

    segment(
        &store.root().join("segments/000001.mkv"),
        "testsrc=size=320x240:rate=10:duration=5",
        "sine=frequency=440:duration=5",
    );
    segment(
        &store.root().join("segments/000002.mkv"),
        "color=c=black:s=320x240:r=10:d=5",
        "anullsrc=r=48000:cl=stereo:d=5",
    );
    for (ord, a, b) in [(1u32, 0.0, 5.0), (2, 5.0, 10.0)] {
        store
            .commit_segment(SegmentRecord {
                id: SegmentId(ord),
                path: format!("segments/{ord:06}.mkv"),
                start: MediaTime::from_secs(a, HZ_1K).expect("time"),
                end: MediaTime::from_secs(b, HZ_1K).expect("time"),
            })
            .expect("commit");
    }
    store.root().to_path_buf()
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().expect("run binary")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
#[ignore = "needs host ffmpeg"]
fn audio_idle_and_project_agree_on_a_synthetic_session() {
    if !have_ffmpeg() {
        eprintln!("skipping: no host ffmpeg");
        return;
    }
    let session = synthetic_session();
    let dir = session.to_string_lossy().into_owned();

    let audio = run(&["audio", &dir]);
    assert!(
        audio.status.success(),
        "{}",
        String::from_utf8_lossy(&audio.stderr)
    );
    let text = stdout(&audio);
    assert!(text.contains("4 extracted"), "{text}");
    assert!(text.contains("microphone\ta:1"), "{text}");

    // Idle from measured signals — this session has no pointer log at all.
    let idle = run(&["idle", &dir, "--threshold", "2"]);
    assert!(
        idle.status.success(),
        "{}",
        String::from_utf8_lossy(&idle.stderr)
    );
    let text = stdout(&idle);
    assert!(text.contains("agreed by: motion:video"), "{text}");
    assert!(text.contains("audio_level:audio:system"), "{text}");
    assert!(!text.starts_with("0 idle"), "{text}");

    let out = session.join("project.json");
    let project = run(&["project", &dir, "-o", &out.to_string_lossy()]);
    assert!(
        project.status.success(),
        "{}",
        String::from_utf8_lossy(&project.stderr)
    );
    let doc = std::fs::read_to_string(&out).expect("project.json");
    assert!(doc.contains("\"version\": 1"), "{doc}");
    assert!(
        doc.contains(".m4a"),
        "audio must be addressed by file: {doc}"
    );
    assert!(!doc.contains("audio_resolved"), "{doc}");
    assert!(
        String::from_utf8_lossy(&project.stderr).is_empty(),
        "no unresolved-audio warning expected"
    );

    let _ = std::fs::remove_dir_all(session);
}

#[test]
#[ignore = "needs host ffmpeg"]
fn project_without_demux_flags_audio_and_warns() {
    if !have_ffmpeg() {
        eprintln!("skipping: no host ffmpeg");
        return;
    }
    let session = synthetic_session();
    let dir = session.to_string_lossy().into_owned();
    let out = session.join("project.json");

    let project = run(&[
        "project",
        &dir,
        "-o",
        &out.to_string_lossy(),
        "--no-audio-extract",
    ]);
    assert!(project.status.success());
    let doc = std::fs::read_to_string(&out).expect("project.json");
    assert!(doc.contains("\"audio_resolved\": \"false\""), "{doc}");
    assert!(doc.contains("\"muted\": true"), "{doc}");
    let warn = String::from_utf8_lossy(&project.stderr);
    assert!(warn.contains("muted"), "{warn}");

    let _ = std::fs::remove_dir_all(session);
}

#[test]
fn idle_remove_refuses_a_session_with_no_evidence() {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("rf-cli-blind-{n}"));
    let store = SessionStore::create(
        &root,
        SessionMeta {
            id: SessionId::new("ses_blind"),
            name: "blind".into(),
            spec: CaptureSpec::screen(),
            started_unix: None,
            duration: None,
        },
    )
    .expect("create session");
    let dir = store.root().to_string_lossy().into_owned();

    let out = run(&["idle", &dir, "--remove", "--no-motion", "--no-audio"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("refused"), "{err}");

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn emit_media_prints_committed_paths_by_session_id() {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let parent = std::env::temp_dir().join(format!("rf-cli-emit-{n}"));
    let mut store = SessionStore::create(
        &parent,
        SessionMeta {
            id: SessionId::new("ses_emit"),
            name: "emit".into(),
            spec: CaptureSpec::screen(),
            started_unix: None,
            duration: None,
        },
    )
    .expect("create session");
    // Loose file a Host glob of sessions/<id>/ would pick up.
    std::fs::write(store.root().join("segments/000099.mkv"), b"tail").unwrap();
    store
        .commit_segment(SegmentRecord {
            id: SegmentId(1),
            path: "segments/000001.mkv".into(),
            start: MediaTime::from_secs(0.0, HZ_1K).expect("time"),
            end: MediaTime::from_secs(5.0, HZ_1K).expect("time"),
        })
        .expect("commit");
    std::fs::write(store.root().join("segments/000001.mkv"), b"seg").unwrap();

    let parent_s = parent.to_string_lossy().into_owned();
    let listed = run(&["emit-media", "--session", "ses_emit", "--dir", &parent_s]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let text = stdout(&listed);
    assert!(text.contains("000001.mkv"), "{text}");
    assert!(!text.contains("000099"), "{text}");
    let line = text.lines().next().expect("one path");
    assert!(
        Path::new(line).is_absolute(),
        "Host --video needs an absolute path: {line}"
    );

    let json = run(&[
        "emit-media",
        "--session",
        "ses_emit",
        "--dir",
        &parent_s,
        "--json",
    ]);
    assert!(json.status.success());
    let body = stdout(&json);
    assert!(body.contains("\"role\": \"video\""), "{body}");
    assert!(body.contains("\"ticks\": 5000"), "{body}");
    assert!(!body.contains("000099"), "{body}");

    let _ = std::fs::remove_dir_all(parent);
}

#[test]
fn clocks_prints_sidecar_and_skips_repair_when_asked() {
    use reelforge_capture_store::{ClockMaster, ClockSegment};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let parent = std::env::temp_dir().join(format!("rf-cli-clk-{n}"));
    let mut store = SessionStore::create(
        &parent,
        SessionMeta {
            id: SessionId::new("ses_clk"),
            name: "clk".into(),
            spec: CaptureSpec::screen(),
            started_unix: None,
            duration: None,
        },
    )
    .expect("create session");
    store
        .commit_segment(SegmentRecord {
            id: SegmentId(1),
            path: "segments/000001.mkv".into(),
            start: MediaTime::from_secs(0.0, HZ_1K).expect("time"),
            end: MediaTime::from_secs(5.0, HZ_1K).expect("time"),
        })
        .expect("commit");
    store
        .append_clock(ClockSegment {
            id: SegmentId(1),
            start: MediaTime::from_secs(0.0, HZ_1K).expect("time"),
            end: MediaTime::from_secs(5.0, HZ_1K).expect("time"),
            master: ClockMaster::Video,
            session_secs: 5.2,
            video_secs: Some(5.0),
            audio_secs: Some(4.94),
            video_start_secs: None,
            audio: Vec::new(),
            correction_ms: -200,
        })
        .expect("clocks");

    let parent_s = parent.to_string_lossy().into_owned();
    let out = run(&[
        "clocks",
        "--session",
        "ses_clk",
        "--dir",
        &parent_s,
        "--no-repair",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert!(text.contains("video"), "{text}");
    assert!(text.contains("corr=-200ms"), "{text}");
    assert!(text.contains("session=5.200"), "{text}");

    let _ = std::fs::remove_dir_all(parent);
}
