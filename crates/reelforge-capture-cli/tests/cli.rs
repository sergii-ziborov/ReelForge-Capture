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
