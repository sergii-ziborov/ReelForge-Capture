//! `reelforge-capture` — headless screen capture + `CaptureProject`.
#![allow(
    missing_docs,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::needless_pass_by_value,
    clippy::too_many_lines
)]

mod io;

use clap::{Parser, Subcommand};
use io::{click_zoom_path, load_edits, parse_video, save_edits, spec_name};
use reelforge_capture_analyze::{SignalOptions, materialize_audio, session_signals};
use reelforge_capture_core::{
    AudioDevice, AudioMix, CaptureError, CaptureSpec, HZ_1K, MediaTime, Result, SessionId,
    SessionMeta,
};
use reelforge_capture_edit::{
    EditDecision, IdleConfig, apply_ranges, detect_idle_multi, zoom_from_clicks,
};
use reelforge_capture_platform::{grab_command, list_audio_hint, list_windows};
use reelforge_capture_project::{project_from_session, to_json_pretty, unresolved_audio};
use reelforge_capture_runtime::{SessionPhase, SessionSupervisor};
use reelforge_capture_store::SessionStore;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(
    name = "reelforge-capture",
    about = "ReelForge Capture — experimental host-ffmpeg recorder (Windows is the supported grab path)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List windows and ffmpeg audio device dump.
    Devices,
    /// Create a session dir and print (or spawn) the ffmpeg grab.
    Start {
        /// Parent directory for `sessions/<id>`.
        #[arg(long, default_value = "sessions")]
        dir: PathBuf,
        /// Session id.
        #[arg(long, default_value = "ses_1")]
        id: String,
        /// Full desktop.
        #[arg(long)]
        screen: bool,
        /// Window title substring.
        #[arg(long)]
        window: Option<String>,
        /// Region `x,y,w,h`.
        #[arg(long)]
        region: Option<String>,
        /// Microphone (host name / index: dshow, avfoundation, pulse).
        #[arg(long)]
        mic: Option<String>,
        /// System loopback / monitor (host device name).
        #[arg(long)]
        system_audio: Option<String>,
        /// Closed segment length.
        #[arg(long, default_value_t = 5.0)]
        segment_secs: f64,
        /// Supervise the grab (own the process, commit closed segments) until Ctrl+C.
        #[arg(long)]
        run: bool,
        /// With `--run`, stop after N seconds.
        #[arg(long)]
        for_secs: Option<f64>,
    },
    /// Range edits.
    Edit {
        /// Session directory.
        session: PathBuf,
        #[command(subcommand)]
        op: EditCmd,
    },
    /// Demux each audio leg into its own file (`audio/<leg>/…` + `audio.json`).
    Audio {
        /// Session directory.
        session: PathBuf,
    },
    /// Detect idle ranges from pointer + frame difference + audio energy.
    Idle {
        /// Session directory.
        session: PathBuf,
        /// Still-time threshold in seconds.
        #[arg(long, default_value_t = 3.0)]
        threshold: f64,
        /// Append remove decisions for idle ranges.
        #[arg(long)]
        remove: bool,
        /// Do not measure frame difference.
        #[arg(long)]
        no_motion: bool,
        /// Do not measure audio energy.
        #[arg(long)]
        no_audio: bool,
        /// Frame-difference samples per second.
        #[arg(long, default_value_t = 2.0)]
        sample_fps: f64,
        /// Frame difference above this counts as picture activity.
        #[arg(long, default_value_t = 1.0)]
        motion_above: f64,
        /// Audio RMS (dBFS) above this counts as sound activity.
        #[arg(long, default_value_t = -45.0, allow_hyphen_values = true)]
        audio_above_db: f64,
    },
    /// Write click-zoom hints into edits.json.
    ZoomClicks {
        /// Session directory.
        session: PathBuf,
        /// Window length seconds.
        #[arg(long, default_value_t = 0.4)]
        duration: f64,
        /// Scale (`1.8` = 180%).
        #[arg(long, default_value_t = 1.8)]
        scale: f64,
    },
    /// Emit headless `CaptureProject` JSON.
    Project {
        /// Session directory.
        session: PathBuf,
        /// Output path.
        #[arg(short, long)]
        output: PathBuf,
        /// Do not demux audio legs first (audio tracks stay muted / flagged).
        #[arg(long)]
        no_audio_extract: bool,
    },
}

#[derive(Subcommand)]
enum EditCmd {
    /// Keep only `[start, end)`.
    Trim {
        #[arg(long)]
        start: f64,
        #[arg(long)]
        end: f64,
    },
    /// Drop `[start, end)`.
    Remove {
        #[arg(long)]
        start: f64,
        #[arg(long)]
        end: f64,
    },
    /// Speed factor on `[start, end)`.
    Speed {
        #[arg(long)]
        start: f64,
        #[arg(long)]
        end: f64,
        #[arg(long)]
        factor: f64,
    },
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Devices => devices(),
        Cmd::Start {
            dir,
            id,
            screen,
            window,
            region,
            mic,
            system_audio,
            segment_secs,
            run,
            for_secs,
        } => start(
            dir,
            id,
            screen,
            window,
            region,
            mic,
            system_audio,
            segment_secs,
            run,
            for_secs,
        ),
        Cmd::Edit { session, op } => edit(&session, op),
        Cmd::Audio { session } => audio(&session),
        Cmd::Idle {
            session,
            threshold,
            remove,
            no_motion,
            no_audio,
            sample_fps,
            motion_above,
            audio_above_db,
        } => idle(
            &session,
            remove,
            IdleConfig {
                threshold: MediaTime::from_secs(threshold, HZ_1K)?,
                motion_above,
                audio_above_db,
            },
            SignalOptions {
                motion: !no_motion,
                audio: !no_audio,
                sample_fps,
                ..SignalOptions::default()
            },
        ),
        Cmd::ZoomClicks {
            session,
            duration,
            scale,
        } => zoom(&session, duration, scale),
        Cmd::Project {
            session,
            output,
            no_audio_extract,
        } => emit_project(&session, &output, !no_audio_extract),
    }
}

fn devices() -> Result<()> {
    for w in list_windows()? {
        println!("window\t{}\t{}", w.process, w.title);
    }
    let audio = list_audio_hint()?;
    if audio.raw.is_empty() {
        println!("audio\t(host audio list unavailable)");
    } else {
        print!("{}", audio.raw);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn start(
    dir: PathBuf,
    id: String,
    screen: bool,
    window: Option<String>,
    region: Option<String>,
    mic: Option<String>,
    system_audio: Option<String>,
    segment_secs: f64,
    run: bool,
    for_secs: Option<f64>,
) -> Result<()> {
    if for_secs.is_some() && !run {
        return Err(CaptureError::message("--for-secs requires --run"));
    }
    if let Some(secs) = for_secs
        && !(secs.is_finite() && secs > 0.0)
    {
        return Err(CaptureError::message("--for-secs must be > 0"));
    }
    let video = parse_video(screen, window, region)?;
    let mut spec = CaptureSpec::screen();
    spec.video = video;
    spec.segment_secs = segment_secs;
    spec.audio = AudioMix {
        system: system_audio.map(AudioDevice::named),
        microphone: mic.map(AudioDevice::named),
    };
    let meta = SessionMeta {
        id: SessionId::new(id),
        name: spec_name(&spec),
        spec: spec.clone(),
        started_unix: None,
        duration: None,
    };
    if run {
        return run_supervised(dir, meta, for_secs);
    }
    let store = SessionStore::create(&dir, meta)?;
    let grab = grab_command(&spec, store.root())?;
    print!("{} ", grab.program);
    for a in &grab.args {
        if a.contains(' ') {
            print!("\"{a}\" ");
        } else {
            print!("{a} ");
        }
    }
    println!();
    println!("session {}", store.root().display());
    Ok(())
}

fn run_supervised(dir: PathBuf, meta: SessionMeta, for_secs: Option<f64>) -> Result<()> {
    let mut sup = SessionSupervisor::start(&dir, meta)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_h = Arc::clone(&stop);
    if let Err(e) = ctrlc::set_handler(move || stop_h.store(true, Ordering::SeqCst)) {
        eprintln!("warning: no Ctrl+C handler ({e})");
    }
    let deadline = for_secs.map(|s| Instant::now() + Duration::from_secs_f64(s));
    println!(
        "recording {} (Ctrl+C to stop)",
        sup.store().root().display()
    );
    loop {
        for ev in sup.tick()? {
            println!("{ev}");
        }
        let phase = sup.phase();
        if matches!(phase, SessionPhase::Failed | SessionPhase::Stopped) {
            break;
        }
        let timed_out = deadline.is_some_and(|d| Instant::now() >= d);
        if stop.load(Ordering::SeqCst) || timed_out {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    if matches!(sup.phase(), SessionPhase::Recording | SessionPhase::Paused) {
        let _ = sup.stop()?;
        println!("stopped");
    }
    let st = sup.status();
    println!(
        "session {}  segments={}  duration={:.3}s  {:?}",
        st.id.as_str(),
        st.committed_segments,
        st.closed_duration.as_secs(),
        st.phase
    );
    if let Some(err) = st.last_error {
        eprintln!("{err}");
        return Err(CaptureError::message(err));
    }
    Ok(())
}

fn edit(session: &Path, op: EditCmd) -> Result<()> {
    let mut list = load_edits(session)?;
    let dec = match op {
        EditCmd::Trim { start, end } => EditDecision::Trim {
            start: MediaTime::from_secs(start, HZ_1K)?,
            end: MediaTime::from_secs(end, HZ_1K)?,
        },
        EditCmd::Remove { start, end } => EditDecision::Remove {
            start: MediaTime::from_secs(start, HZ_1K)?,
            end: MediaTime::from_secs(end, HZ_1K)?,
        },
        EditCmd::Speed { start, end, factor } => EditDecision::Speed {
            start: MediaTime::from_secs(start, HZ_1K)?,
            end: MediaTime::from_secs(end, HZ_1K)?,
            factor,
        },
    };
    list.ops.push(dec);
    save_edits(session, &list)
}

fn audio(session: &Path) -> Result<()> {
    let store = SessionStore::open(session)?;
    if !store.manifest().meta.spec.audio.has_any() {
        println!("no audio legs configured for this session");
        return Ok(());
    }
    let done = materialize_audio(&store)?;
    println!(
        "{} extracted, {} reused → {}",
        done.extracted,
        done.reused,
        store.audio_sidecar_path().display()
    );
    for leg in &done.sidecar.legs {
        println!(
            "{}\ta:{}\t{} file(s)",
            leg.leg.as_str(),
            leg.audio_index,
            leg.files.len()
        );
    }
    for f in &done.failures {
        eprintln!("warning: {f}");
    }
    Ok(())
}

fn idle(session: &Path, remove: bool, config: IdleConfig, signals: SignalOptions) -> Result<()> {
    let store = SessionStore::open(session)?;
    let events = store.load_events()?;
    let tracks = session_signals(&store, &signals)?;
    let report = detect_idle_multi(&events, &tracks, store.closed_duration(), config)?;

    if report.is_blind() {
        if remove {
            return Err(CaptureError::message(
                "idle --remove refused: no evidence (no pointer log, no measurable signal) — \
                 that would propose deleting the whole session",
            ));
        }
        println!("0 idle range(s) (no evidence; not treating the session as idle)");
        return Ok(());
    }

    println!(
        "{} idle range(s) agreed by: {}",
        report.ranges.len(),
        report.sources.join(" + ")
    );
    if !remove {
        for r in report.ranges {
            println!("{:.3}..{:.3}", r.start.as_secs(), r.end.as_secs());
        }
        return Ok(());
    }
    let mut list = load_edits(session)?;
    for r in report.ranges {
        list.ops.push(EditDecision::Remove {
            start: r.start,
            end: r.end,
        });
    }
    save_edits(session, &list)
}

fn zoom(session: &Path, duration: f64, scale: f64) -> Result<()> {
    let store = SessionStore::open(session)?;
    let events = store.load_events()?;
    let zooms = zoom_from_clicks(
        &events,
        MediaTime::from_secs(duration, HZ_1K)?,
        scale,
        store.closed_duration(),
    )?;
    let path = click_zoom_path(session);
    fs::write(&path, serde_json::to_string_pretty(&zooms)?)?;
    println!(
        "{} click-zoom marker(s) → {} (markers only; not crop/scale keyframes)",
        zooms.len(),
        path.display()
    );
    Ok(())
}

fn emit_project(session: &Path, output: &Path, extract_audio: bool) -> Result<()> {
    let store = SessionStore::open(session)?;
    if extract_audio && !unresolved_audio(&store)?.is_empty() {
        match materialize_audio(&store) {
            Ok(done) => println!(
                "audio: {} extracted, {} reused",
                done.extracted, done.reused
            ),
            Err(e) => eprintln!("warning: audio demux failed ({e})"),
        }
    }
    let list = load_edits(session)?;
    let dur = store.closed_duration();
    let kept = if dur.ticks > 0 {
        apply_ranges(dur, &list)?
    } else {
        Vec::new()
    };
    let zoom_path = click_zoom_path(session);
    let zooms = if zoom_path.is_file() {
        serde_json::from_str(&fs::read_to_string(zoom_path)?)?
    } else {
        Vec::new()
    };
    let project = project_from_session(&store, &kept, &zooms)?;
    fs::write(output, to_json_pretty(&project)?)?;
    println!("wrote {}", output.display());
    for leg in unresolved_audio(&store)? {
        eprintln!(
            "warning: {} audio track is muted — no demuxed file (run `reelforge-capture audio {}`)",
            leg.as_str(),
            session.display()
        );
    }
    Ok(())
}
