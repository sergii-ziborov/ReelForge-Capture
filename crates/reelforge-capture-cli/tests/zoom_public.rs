#![allow(
    clippy::many_single_char_names,
    clippy::too_many_lines,
    clippy::cloned_ref_to_slice_refs
)]
//! Before/after click-zoom on a public-domain clip (Big Buck Bunny 320×180).
//!
//! ```text
//! cargo test -p reelforge-capture-cli --test zoom_public -- --ignored --nocapture
//! ```

use reelforge_capture_core::{
    CaptureSpec, ClickButton, HZ_1K, MediaRange, MediaTime, PointerEvent, SegmentId, SessionId,
    SessionMeta,
};
use reelforge_capture_edit::{ClickZoom, crop_around, zoom_slices};
use reelforge_capture_platform::{probe_duration, probe_video_size, render_crop_scale};
use reelforge_capture_project::project_from_session_sized;
use reelforge_capture_store::{SegmentRecord, SessionStore};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const CANDIDATES: &[&str] = &[
    "https://test-videos.co.uk/vids/bigbuckbunny/mp4/h264/360/Big_Buck_Bunny_360_10s_1MB.mp4",
    "https://commondatastorage.googleapis.com/gtv-videos-bucket/sample/ForBiggerBlazes.mp4",
    "https://www.w3schools.com/html/mov_bbb.mp4",
];

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn cache_public_clip() -> PathBuf {
    let dest = std::env::temp_dir().join("rf-public-bbb.mp4");
    if dest.is_file() && dest.metadata().is_ok_and(|m| m.len() > 10_000) {
        return dest;
    }
    let tmp = dest.with_extension("part");
    for url in CANDIDATES {
        let curl = Command::new("curl")
            .args(["-fsSL", "--retry", "2", "-o"])
            .arg(&tmp)
            .arg(url)
            .status();
        if curl.is_ok_and(|s| s.success()) && tmp.metadata().is_ok_and(|m| m.len() > 10_000) {
            fs::rename(&tmp, &dest).expect("rename download");
            return dest;
        }
        let ps = format!(
            "Invoke-WebRequest -Uri '{url}' -OutFile '{}' -UseBasicParsing",
            tmp.display()
        );
        let ok = Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps])
            .status()
            .is_ok_and(|s| s.success());
        if ok && tmp.metadata().is_ok_and(|m| m.len() > 10_000) {
            fs::rename(&tmp, &dest).expect("rename download");
            return dest;
        }
    }
    panic!("could not download a public sample clip from {CANDIDATES:?}");
}

fn extract_frame(input: &Path, at: f64, out: &Path) {
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-y", "-loglevel", "error"])
        .args(["-ss", &format!("{at}"), "-i"])
        .arg(input)
        .args(["-frames:v", "1"])
        .arg(out)
        .status()
        .expect("ffmpeg frame");
    assert!(status.success(), "frame extract failed {}", out.display());
}

#[test]
#[ignore = "needs host ffmpeg + network (public Big Buck Bunny clip)"]
fn public_clip_zoom_before_and_after_differ() {
    assert!(have_ffmpeg(), "ffmpeg required");
    let src = cache_public_clip();
    let (w, h) = probe_video_size(&src).unwrap().expect("probe size");
    assert!(w >= 160 && h >= 90, "clip too small: {w}x{h}");

    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("rf-zoom-pub-{n}"));
    let mut store = SessionStore::create(
        &root,
        SessionMeta {
            id: SessionId::new("ses_bbb"),
            name: "bbb".into(),
            spec: CaptureSpec::screen(),
            started_unix: None,
            duration: None,
        },
    )
    .unwrap();
    let seg = store.root().join("segments/000001.mkv");
    fs::copy(&src, &seg).unwrap();
    let dur = probe_duration(&seg)
        .unwrap()
        .unwrap_or_else(|| MediaTime::from_secs(10.0, HZ_1K).unwrap());
    store
        .commit_segment(SegmentRecord {
            id: SegmentId::first(),
            path: "segments/000001.mkv".into(),
            start: MediaTime::zero(HZ_1K),
            end: dur,
        })
        .unwrap();

    // A click on the right half — Bunny is usually around here in the 320×180 encode.
    let click_t = 2.0_f64;
    let click = (
        i32::try_from(w * 3 / 4).unwrap(),
        i32::try_from(h / 2).unwrap(),
    );
    store
        .append_event(&PointerEvent::Click {
            t: MediaTime::from_secs(click_t, HZ_1K).unwrap(),
            x: click.0,
            y: click.1,
            button: ClickButton::Left,
        })
        .unwrap();

    let zoom = ClickZoom {
        range: MediaRange::new(
            MediaTime::from_secs(click_t, HZ_1K).unwrap(),
            MediaTime::from_secs(click_t + 1.0, HZ_1K).unwrap(),
        )
        .unwrap(),
        x: click.0,
        y: click.1,
        scale: 2.0,
    };
    let kept = [reelforge_capture_edit::KeptRange {
        source: MediaRange::new(MediaTime::zero(HZ_1K), dur).unwrap(),
        speed: 1.0,
    }];
    let project = project_from_session_sized(&store, &kept, &[zoom.clone()], Some((w, h))).unwrap();
    let seq = project.active().unwrap();
    let zoomed = seq.tracks[0].items.iter().filter_map(|i| match i {
        reelforge_capture_schema::TimelineItem::Clip(c) if c.crop.is_some() => Some(c),
        _ => None,
    });
    assert!(zoomed.clone().count() >= 1, "expected cropped clips");

    let slices = zoom_slices(&[zoom], w, h).unwrap();
    let hold = slices
        .iter()
        .max_by(|a, b| a.scale.partial_cmp(&b.scale).unwrap())
        .unwrap();
    let crop = hold.crop;
    assert_eq!(crop, crop_around(click.0, click.1, 2.0, w, h));

    let before = store.root().join("before.mp4");
    let after = store.root().join("after.mp4");
    let window = 0.4;
    // Identity window (no crop).
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-y", "-loglevel", "error"])
        .args(["-ss", &format!("{click_t}"), "-t", &format!("{window}")])
        .arg("-i")
        .arg(&seg)
        .args(["-an", "-c:v", "libx264", "-preset", "ultrafast"])
        .arg(&before)
        .status()
        .unwrap();
    assert!(status.success(), "before encode failed");
    render_crop_scale(
        &seg,
        &after,
        click_t,
        window,
        (crop.x, crop.y, crop.w, crop.h),
        (w, h),
    )
    .unwrap();

    let fb = store.root().join("before.png");
    let fa = store.root().join("after.png");
    extract_frame(&before, 0.2, &fb);
    extract_frame(&after, 0.2, &fa);
    let b = fs::read(&fb).unwrap();
    let a = fs::read(&fa).unwrap();
    assert_ne!(b, a, "zoomed frame must differ from the uncropped one");
    assert!(after.metadata().unwrap().len() > 1000);

    println!(
        "before {}  after {}  crop={}x{}+{}+{}  frames {} vs {} bytes",
        before.display(),
        after.display(),
        crop.w,
        crop.h,
        crop.x,
        crop.y,
        b.len(),
        a.len()
    );
    // Leave the dir for inspection; temp will be cleaned by the OS.
}
