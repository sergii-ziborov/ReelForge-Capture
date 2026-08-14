//! WAL recover: unfinished tail is dropped; events persist.

use reelforge_capture_core::{
    CaptureSpec, ClickButton, HZ_1K, MediaTime, PointerEvent, SegmentId, SessionId, SessionMeta,
};
use reelforge_capture_store::{SegmentRecord, SessionStore};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn tmp_root() -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rf-cap-{n}"))
}

fn meta(id: &str) -> SessionMeta {
    SessionMeta {
        id: SessionId::new(id),
        name: "t".into(),
        spec: CaptureSpec::screen(),
        started_unix: None,
        duration: None,
    }
}

#[test]
fn crash_drops_uncommitted_segment() {
    let root = tmp_root();
    let mut s = SessionStore::create(&root, meta("ses_1")).unwrap();
    let start = MediaTime::zero(HZ_1K);
    s.begin_segment(SegmentId::first(), "segments/000001.mkv", start)
        .unwrap();
    s.commit_segment(SegmentRecord {
        id: SegmentId::first(),
        path: "segments/000001.mkv".into(),
        start,
        end: MediaTime::from_secs(5.0, HZ_1K).unwrap(),
    })
    .unwrap();
    s.begin_segment(
        SegmentId(2),
        "segments/000002.mkv",
        MediaTime::from_secs(5.0, HZ_1K).unwrap(),
    )
    .unwrap();
    drop(s);

    let opened = SessionStore::open(root.join("ses_1")).unwrap();
    assert_eq!(opened.manifest().segments.len(), 1);
    assert_eq!(opened.closed_duration().ticks, 5000);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn events_survive() {
    let root = tmp_root();
    let s = SessionStore::create(&root, meta("ses_e")).unwrap();
    s.append_event(&PointerEvent::Click {
        t: MediaTime::from_secs(0.2, HZ_1K).unwrap(),
        x: 10,
        y: 20,
        button: ClickButton::Left,
    })
    .unwrap();
    let ev = s.load_events().unwrap();
    assert_eq!(ev.len(), 1);
    assert!(ev[0].is_click());
    let _ = std::fs::remove_dir_all(root);
}
