//! Wall-clock budget of the post-capture path. Not a microbenchmark harness —
//! it answers one question: *what does a finished session cost to analyze?*
//!
//! ```text
//! cargo test -p reelforge-capture-analyze --release --test bench -- --ignored --nocapture
//! ```
//!
//! Two halves:
//!
//! * **host ffmpeg** — demux and measurement, reported as a multiple of
//!   realtime (`30×` means a 30 s recording is analyzed in 1 s);
//! * **pure CPU** — idle agreement and project authoring on an hour-long
//!   session, where nothing shells out.

use reelforge_capture_analyze::{SignalOptions, materialize_audio, session_signals};
use reelforge_capture_core::{
    AudioDevice, CaptureSpec, HZ_1K, MediaRange, MediaTime, PointerEvent, SegmentId, SessionId,
    SessionMeta, SignalKind, SignalTrack,
};
use reelforge_capture_edit::{IdleConfig, KeptRange, detect_idle_multi};
use reelforge_capture_project::{project_from_session, to_json_pretty};
use reelforge_capture_store::{
    AudioLegTrack, AudioSegmentFile, AudioSidecar, SegmentRecord, SessionStore,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Segment length used by the default capture spec.
const SEG_SECS: f64 = 5.0;
/// Segments in the ffmpeg half (kept small: every one is encoded first).
const SEG_COUNT: u32 = 6;
/// Session length for the pure-CPU half.
const HOUR: f64 = 3600.0;
/// Segments in an hour at [`SEG_SECS`].
const HOUR_SEGMENTS: u32 = 720;
/// Signal sampling period and its sample count over an hour.
const SIGNAL_PERIOD: f64 = 0.5;
const SIGNAL_SAMPLES: u32 = 7_200;
/// Pointer heartbeat and its sample count over an hour.
const POINTER_PERIOD: f64 = 0.25;
const POINTER_SAMPLES: u32 = 14_400;

fn t(secs: f64) -> MediaTime {
    MediaTime::from_secs(secs, HZ_1K).expect("media time")
}

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn tmp(prefix: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{n}"))
}

fn time<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let out = f();
    (out, start.elapsed())
}

fn report(label: &str, took: Duration, media_secs: f64) {
    let secs = took.as_secs_f64();
    let speed = if secs > 0.0 {
        media_secs / secs
    } else {
        f64::NAN
    };
    println!("{label:<34} {secs:>9.3} s   {speed:>7.1}x realtime");
}

/// 720p screen-sized segment with two audio legs, as a real capture writes it.
#[rustfmt::skip]
fn write_segment(out: &Path, busy: bool) {
    let video = if busy {
        format!("testsrc2=size=1280x720:rate=30:duration={SEG_SECS}")
    } else {
        format!("color=c=black:s=1280x720:r=30:d={SEG_SECS}")
    };
    let audio = if busy {
        format!("sine=frequency=440:duration={SEG_SECS}")
    } else {
        format!("anullsrc=r=48000:cl=stereo:d={SEG_SECS}")
    };
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-y"])
        .args(["-f", "lavfi", "-i", &video])
        .args(["-f", "lavfi", "-i", &audio])
        .args(["-f", "lavfi", "-i", &audio])
        .args(["-map", "0:v", "-map", "1:a", "-map", "2:a"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"])
        .args(["-c:a", "aac"])
        .arg(out)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed for {}", out.display());
}

fn media_session() -> (PathBuf, SessionStore) {
    let root = tmp("rf-bench");
    let mut spec = CaptureSpec::screen();
    spec.audio.system = Some(AudioDevice::named("loop"));
    spec.audio.microphone = Some(AudioDevice::named("mic"));
    let mut store = SessionStore::create(
        &root,
        SessionMeta {
            id: SessionId::new("ses_bench"),
            name: "bench".into(),
            spec,
            started_unix: None,
            duration: None,
        },
    )
    .expect("create session");

    for ord in 1..=SEG_COUNT {
        let rel = format!("segments/{ord:06}.mkv");
        write_segment(&store.root().join(&rel), ord % 2 == 1);
        let start = f64::from(ord - 1) * SEG_SECS;
        store
            .commit_segment(SegmentRecord {
                id: SegmentId(ord),
                path: rel,
                start: t(start),
                end: t(start + SEG_SECS),
            })
            .expect("commit");
    }
    (root, store)
}

#[test]
#[ignore = "benchmark; needs host ffmpeg"]
fn host_ffmpeg_analysis_budget() {
    if !have_ffmpeg() {
        eprintln!("skipping: no host ffmpeg");
        return;
    }
    let media_secs = f64::from(SEG_COUNT) * SEG_SECS;
    println!(
        "\n== host ffmpeg ==  {SEG_COUNT} × {SEG_SECS}s 720p30, 2 audio legs ({media_secs}s media)"
    );
    let (root, store) = media_session();

    let (done, took) = time(|| materialize_audio(&store).expect("materialize"));
    assert_eq!(
        done.extracted,
        usize::try_from(SEG_COUNT).expect("fits") * 2
    );
    report("demux 2 audio legs (-c copy)", took, media_secs);

    let (_, took) = time(|| materialize_audio(&store).expect("materialize"));
    report("demux again (already on disk)", took, media_secs);

    let motion_only = SignalOptions {
        audio: false,
        ..SignalOptions::default()
    };
    let (tracks, took) = time(|| session_signals(&store, &motion_only).expect("signals"));
    assert_eq!(tracks.len(), 1);
    report("measure frame difference", took, media_secs);

    let audio_only = SignalOptions {
        motion: false,
        ..SignalOptions::default()
    };
    let (tracks, took) = time(|| session_signals(&store, &audio_only).expect("signals"));
    assert_eq!(tracks.len(), 2);
    report("measure audio energy (2 legs)", took, media_secs);

    let (tracks, took) =
        time(|| session_signals(&store, &SignalOptions::default()).expect("signals"));
    assert_eq!(tracks.len(), 3);
    report("measure both (idle input)", took, media_secs);

    let samples: usize = tracks.iter().map(|t| t.samples.len()).sum();
    println!("{:<34} {samples} samples", "signal volume");

    let _ = std::fs::remove_dir_all(root);
}

/// Alternating 60 s quiet / 15 s busy, the shape a real screencast has.
fn busy_at(secs: f64) -> bool {
    secs % 75.0 >= 60.0
}

fn synthetic_track(kind: SignalKind, source: &str, quiet: f64, loud: f64) -> SignalTrack {
    let mut track = SignalTrack::new(kind, source, t(SIGNAL_PERIOD));
    for i in 0..SIGNAL_SAMPLES {
        let at = f64::from(i) * SIGNAL_PERIOD;
        track.push(t(at), if busy_at(at) { loud } else { quiet });
    }
    track
}

fn synthetic_pointer() -> Vec<PointerEvent> {
    let mut out = Vec::new();
    let mut x = 0;
    for i in 0..POINTER_SAMPLES {
        let at = f64::from(i) * POINTER_PERIOD;
        if busy_at(at) {
            x = (x + 7) % 1920;
        }
        out.push(PointerEvent::Cursor {
            t: t(at),
            x,
            y: 540,
        });
    }
    out
}

#[test]
#[ignore = "benchmark"]
#[allow(clippy::too_many_lines)]
fn pure_cpu_budget_on_an_hour_long_session() {
    println!("\n== pure CPU ==  1 h session, 720 segments, no ffmpeg");

    let motion = synthetic_track(SignalKind::Motion, "video", 0.2, 12.0);
    let system = synthetic_track(SignalKind::AudioLevel, "audio:system", -95.0, -18.0);
    let mic = synthetic_track(SignalKind::AudioLevel, "audio:microphone", -95.0, -22.0);
    let events = synthetic_pointer();
    println!(
        "{:<34} {} motion + {} audio + {} pointer",
        "input",
        motion.samples.len(),
        system.samples.len() + mic.samples.len(),
        events.len()
    );

    let tracks = [motion, system, mic];
    let (report_out, took) = time(|| {
        detect_idle_multi(&events, &tracks, t(HOUR), IdleConfig::new(t(3.0))).expect("idle")
    });
    println!(
        "{:<34} {:>9.3} s   {} range(s) from {} source(s)",
        "detect_idle_multi",
        took.as_secs_f64(),
        report_out.ranges.len(),
        report_out.sources.len()
    );
    assert!(!report_out.ranges.is_empty(), "expected idle stretches");

    // Author a project over the same hour: 720 segments, 2 demuxed audio legs,
    // one kept range per idle gap.
    let root = tmp("rf-bench-cpu");
    let mut spec = CaptureSpec::screen();
    spec.audio.system = Some(AudioDevice::named("loop"));
    spec.audio.microphone = Some(AudioDevice::named("mic"));
    let mut store = SessionStore::create(
        &root,
        SessionMeta {
            id: SessionId::new("ses_cpu"),
            name: "cpu".into(),
            spec,
            started_unix: None,
            duration: None,
        },
    )
    .expect("create session");

    let mut sidecar = AudioSidecar::new();
    let mut legs = [
        AudioLegTrack {
            leg: reelforge_capture_core::AudioLeg::System,
            device: "loop".into(),
            audio_index: 0,
            files: Vec::new(),
        },
        AudioLegTrack {
            leg: reelforge_capture_core::AudioLeg::Microphone,
            device: "mic".into(),
            audio_index: 1,
            files: Vec::new(),
        },
    ];
    for ord in 1..=HOUR_SEGMENTS {
        let start = f64::from(ord - 1) * SEG_SECS;
        store
            .commit_segment(SegmentRecord {
                id: SegmentId(ord),
                path: format!("segments/{ord:06}.mkv"),
                start: t(start),
                end: t(start + SEG_SECS),
            })
            .expect("commit");
        for leg in &mut legs {
            leg.files.push(AudioSegmentFile {
                segment: SegmentId(ord),
                path: format!("audio/{}/{ord:06}.m4a", leg.leg.as_str()),
                start: t(start),
                end: t(start + SEG_SECS),
                duration: Some(t(SEG_SECS)),
                gap: None,
            });
        }
    }
    sidecar.legs = legs.to_vec();
    store.write_audio_sidecar(&sidecar).expect("sidecar");

    // Keep everything the idle pass did not propose removing.
    let mut kept = Vec::new();
    let mut cursor = 0i64;
    for idle in &report_out.ranges {
        if idle.start.ticks > cursor {
            kept.push(KeptRange {
                source: MediaRange::new(
                    MediaTime {
                        ticks: cursor,
                        timescale: HZ_1K,
                    },
                    idle.start,
                )
                .expect("range"),
                speed: 1.0,
            });
        }
        cursor = idle.end.ticks;
    }
    if cursor < t(HOUR).ticks {
        kept.push(KeptRange {
            source: MediaRange::new(
                MediaTime {
                    ticks: cursor,
                    timescale: HZ_1K,
                },
                t(HOUR),
            )
            .expect("range"),
            speed: 1.0,
        });
    }
    println!("{:<34} {} kept range(s)", "edit list", kept.len());

    let (project, took) = time(|| project_from_session(&store, &kept, &[]).expect("project"));
    let clips: usize = project
        .active()
        .expect("sequence")
        .tracks
        .iter()
        .map(|t| t.items.len())
        .sum();
    println!(
        "{:<34} {:>9.3} s   {clips} clip(s), {} media",
        "project_from_session (+validate)",
        took.as_secs_f64(),
        project.media.len()
    );

    let (json, took) = time(|| to_json_pretty(&project).expect("json"));
    println!(
        "{:<34} {:>9.3} s   {} KB",
        "serialize CaptureProject",
        took.as_secs_f64(),
        json.len() / 1024
    );

    let (parsed, took) = time(|| reelforge_capture_project::parse_project(&json).expect("parse"));
    println!(
        "{:<34} {:>9.3} s",
        "parse CaptureProject",
        took.as_secs_f64()
    );
    assert_eq!(parsed, project);

    let _ = std::fs::remove_dir_all(root);
}
