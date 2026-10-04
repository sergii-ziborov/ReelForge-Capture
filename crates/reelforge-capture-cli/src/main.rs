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
use io::{click_zoom_path, load_edits, parse_video, resolve_session, save_edits, spec_name};
use reelforge_capture_analyze::{
    SignalOptions, WaveformOptions, materialize_audio, session_signals, session_waveforms,
};
use reelforge_capture_core::{
    AudioDevice, AudioMix, CaptureError, CaptureSpec, HZ_1K, MediaTime, Result, SessionId,
    SessionMeta,
};
use reelforge_capture_edit::{
    EditDecision, IdleConfig, apply_ranges, detect_idle_multi, zoom_from_clicks,
};
use reelforge_capture_platform::{grab_command, list_audio_hint, list_windows};
use reelforge_capture_project::{
    ingest_audio_media, ingest_video_media, project_from_session, to_json_pretty, unresolved_audio,
};
use reelforge_capture_runtime::{LiveStatus, SessionPhase, SessionSupervisor, repair_clocks};
use reelforge_capture_store::{ControlOp, SessionStore};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
        /// Session id. A unique `ses_<unix_ms>_<hex>` is used when omitted.
        #[arg(long)]
        id: Option<String>,
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
    /// Write min/max audio peaks to `waveforms.json` (no extra ffmpeg at UI time).
    Waveform {
        /// Session directory.
        session: PathBuf,
        /// Bucket width in seconds.
        #[arg(long, default_value_t = 0.05)]
        bucket_secs: f64,
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
    /// Print committed media paths Host feeds to `ingest_video`.
    ///
    /// One absolute path per line (video segments, record order). Does not
    /// glob `sessions/<id>/` — only WAL-committed files are listed. Duration
    /// lives on the same entries in the project JSON (`media[].duration`).
    EmitMedia {
        /// Session id (`ses_1`) or an existing session directory.
        #[arg(long)]
        session: String,
        /// Parent directory used when `--session` is an id.
        #[arg(long, default_value = "sessions")]
        dir: PathBuf,
        /// Include demuxed audio files after the video list.
        #[arg(long)]
        all: bool,
        /// Print the `MediaRef` array (uri + duration + role) instead of paths.
        #[arg(long)]
        json: bool,
    },
    /// Print (and backfill) per-segment session / video / audio clocks.
    Clocks {
        /// Session id (`ses_1`) or an existing session directory.
        #[arg(long)]
        session: String,
        /// Parent directory used when `--session` is an id.
        #[arg(long, default_value = "sessions")]
        dir: PathBuf,
        /// Dump `clocks.json` instead of the table.
        #[arg(long)]
        json: bool,
        /// Do not probe committed segments that have no row yet.
        #[arg(long)]
        no_repair: bool,
    },
    /// Ask a live `--run` supervisor to stop (writes `control.json`).
    Stop {
        /// Session directory.
        session: PathBuf,
    },
    /// Ask a live supervisor to pause.
    Pause {
        /// Session directory.
        session: PathBuf,
    },
    /// Ask a paused supervisor to resume.
    Resume {
        /// Session directory.
        session: PathBuf,
    },
    /// Print `status.json` from a live (or just-stopped) session.
    Status {
        /// Session directory.
        session: PathBuf,
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
        Cmd::Waveform {
            session,
            bucket_secs,
        } => waveform(&session, bucket_secs),
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
        Cmd::EmitMedia {
            session,
            dir,
            all,
            json,
        } => emit_media(&session, &dir, all, json),
        Cmd::Clocks {
            session,
            dir,
            json,
            no_repair,
        } => emit_clocks(&session, &dir, json, !no_repair),
        Cmd::Stop { session } => control(&session, ControlOp::Stop),
        Cmd::Pause { session } => control(&session, ControlOp::Pause),
        Cmd::Resume { session } => control(&session, ControlOp::Resume),
        Cmd::Status { session } => print_status(&session),
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
    id: Option<String>,
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
    let id = id.unwrap_or_else(fresh_session_id);
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

/// `ses_{unix_millis}` plus hex from a process-local counter and this counter's address.
fn fresh_session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let addr = std::ptr::from_ref(&COUNTER) as usize as u64;
    let salt = (n.wrapping_shl(16) ^ addr) & 0xffff_ffff;
    format!("ses_{}_{salt:08x}", now.as_millis())
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

fn waveform(session: &Path, bucket_secs: f64) -> Result<()> {
    if !(bucket_secs.is_finite() && bucket_secs > 0.0) {
        return Err(CaptureError::message("--bucket-secs must be > 0"));
    }
    let store = SessionStore::open(session)?;
    if !store.manifest().meta.spec.audio.has_any() {
        println!("no audio legs configured for this session");
        return Ok(());
    }
    let done = session_waveforms(
        &store,
        &WaveformOptions {
            bucket_secs,
            ..WaveformOptions::default()
        },
    )?;
    let peaks: usize = done.sidecar.legs.iter().map(|l| l.peaks.len()).sum();
    println!(
        "{} leg(s), {peaks} peak(s) → {}",
        done.measured,
        store.waveform_path().display()
    );
    for leg in &done.sidecar.legs {
        println!("{}\t{} bucket(s)", leg.leg.as_str(), leg.peaks.len());
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
        "{} click-zoom window(s) → {} (project emits crop+scale; markers kept)",
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

fn emit_media(session: &str, dir: &Path, all: bool, as_json: bool) -> Result<()> {
    let root = resolve_session(session, dir)?;
    let store = SessionStore::open(&root)?;
    let mut media = ingest_video_media(&store)?;
    if all {
        media.extend(ingest_audio_media(&store)?);
    }
    if as_json {
        println!("{}", serde_json::to_string_pretty(&media)?);
        return Ok(());
    }
    for m in media {
        println!("{}", m.uri);
    }
    Ok(())
}

fn emit_clocks(session: &str, dir: &Path, as_json: bool, repair: bool) -> Result<()> {
    let root = resolve_session(session, dir)?;
    let store = SessionStore::open(&root)?;
    if repair {
        let added = repair_clocks(&store)?;
        if added > 0 {
            eprintln!(
                "repaired {added} clock row(s) → {}",
                store.clocks_path().display()
            );
        }
    }
    let side = store.read_clocks()?.unwrap_or_default();
    if as_json {
        println!("{}", serde_json::to_string_pretty(&side)?);
        return Ok(());
    }
    if side.segments.is_empty() {
        println!("(no clocks — run a supervised capture, or drop --no-repair)");
        return Ok(());
    }
    for row in &side.segments {
        let video = row
            .video_secs
            .map_or_else(|| "-".into(), |s| format!("{s:.3}"));
        let audio = row
            .audio
            .iter()
            .map(|a| {
                let d = a
                    .duration_secs
                    .map_or_else(|| "-".into(), |s| format!("{s:.3}"));
                match a.start_secs {
                    Some(st) if st > 0.0 => format!("a{}={d}@{st:.3}", a.index),
                    _ => format!("a{}={d}", a.index),
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        let audio = if audio.is_empty() { "-".into() } else { audio };
        println!(
            "{}\t{}\tsession={:.3}\tvideo={video}\t{audio}\tcorr={}ms",
            row.id.file_stem(),
            row.master.as_str(),
            row.session_secs,
            row.correction_ms
        );
    }
    Ok(())
}

fn control(session: &Path, op: ControlOp) -> Result<()> {
    let store = SessionStore::open(session)?;
    store.write_control(op)?;
    let label = match op {
        ControlOp::Stop => "stop",
        ControlOp::Pause => "pause",
        ControlOp::Resume => "resume",
    };
    println!(
        "queued {label} → {} (the --run supervisor applies it on the next tick)",
        store.control_path().display()
    );
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn print_status(session: &Path) -> Result<()> {
    let path = session.join("status.json");
    if !path.is_file() {
        return Err(CaptureError::message(format!(
            "no status.json in {} (is a supervisor running?)",
            session.display()
        )));
    }
    let live: LiveStatus = serde_json::from_str(&fs::read_to_string(path)?)?;
    println!(
        "{:?}  segments={}  elapsed={:.3}s  closed={:.3}s{}",
        live.phase,
        live.committed_segments,
        live.elapsed_ms as f64 / 1_000.0,
        live.closed_duration_ms as f64 / 1_000.0,
        live.pid.map_or(String::new(), |p| format!("  pid={p}"))
    );
    if let Some(err) = live.last_error {
        eprintln!("{err}");
    }
    Ok(())
}
