//! `reelforge-capture` — headless Windows capture + `CaptureProject`.
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
use reelforge_capture_core::{
    AudioDevice, AudioMix, CaptureSpec, HZ_1K, MediaTime, Result, SessionId, SessionMeta,
};
use reelforge_capture_edit::{EditDecision, apply_ranges, detect_idle, zoom_from_clicks};
use reelforge_capture_platform::{grab_command, list_audio_hint, list_windows};
use reelforge_capture_project::{project_from_session, to_json_pretty};
use reelforge_capture_store::SessionStore;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "reelforge-capture", about = "ReelForge Capture (headless)")]
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
        /// Enable microphone (dshow name).
        #[arg(long)]
        mic: Option<String>,
        /// Enable system loopback (dshow name).
        #[arg(long)]
        system_audio: Option<String>,
        /// Closed segment length.
        #[arg(long, default_value_t = 5.0)]
        segment_secs: f64,
        /// Spawn ffmpeg instead of printing the command.
        #[arg(long)]
        run: bool,
    },
    /// Range edits.
    Edit {
        /// Session directory.
        session: PathBuf,
        #[command(subcommand)]
        op: EditCmd,
    },
    /// Detect idle ranges; `--remove` appends remove ops.
    Idle {
        /// Session directory.
        session: PathBuf,
        /// Still-time threshold in seconds.
        #[arg(long, default_value_t = 3.0)]
        threshold: f64,
        /// Append remove decisions for idle ranges.
        #[arg(long)]
        remove: bool,
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
        ),
        Cmd::Edit { session, op } => edit(&session, op),
        Cmd::Idle {
            session,
            threshold,
            remove,
        } => idle(&session, threshold, remove),
        Cmd::ZoomClicks {
            session,
            duration,
            scale,
        } => zoom(&session, duration, scale),
        Cmd::Project { session, output } => emit_project(&session, &output),
    }
}

fn devices() -> Result<()> {
    for w in list_windows()? {
        println!("window\t{}\t{}", w.process, w.title);
    }
    let audio = list_audio_hint()?;
    if audio.raw.is_empty() {
        println!("audio\t(ffmpeg dshow list unavailable)");
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
) -> Result<()> {
    let video = parse_video(screen, window, region)?;
    let mut spec = CaptureSpec::screen();
    spec.video = video;
    spec.segment_secs = segment_secs;
    spec.audio = AudioMix {
        system: system_audio.map(AudioDevice::dshow),
        microphone: mic.map(AudioDevice::dshow),
    };
    let meta = SessionMeta {
        id: SessionId::new(id),
        name: spec_name(&spec),
        spec: spec.clone(),
        started_unix: None,
        duration: None,
    };
    let store = SessionStore::create(&dir, meta)?;
    let grab = grab_command(&spec, store.root())?;
    if run {
        let _child = reelforge_capture_platform::spawn_grab(&grab)?;
        println!("spawned {} in {}", grab.program, store.root().display());
    } else {
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

fn idle(session: &Path, threshold: f64, remove: bool) -> Result<()> {
    let store = SessionStore::open(session)?;
    let events = store.load_events()?;
    let dur = store.closed_duration();
    let ranges = detect_idle(&events, dur, MediaTime::from_secs(threshold, HZ_1K)?)?;
    println!("{} idle range(s)", ranges.len());
    if !remove {
        for r in ranges {
            println!("{:.3}..{:.3}", r.start.as_secs(), r.end.as_secs());
        }
        return Ok(());
    }
    let mut list = load_edits(session)?;
    for r in ranges {
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
    println!("{} click zoom(s) → {}", zooms.len(), path.display());
    Ok(())
}

fn emit_project(session: &Path, output: &Path) -> Result<()> {
    let store = SessionStore::open(session)?;
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
    Ok(())
}
