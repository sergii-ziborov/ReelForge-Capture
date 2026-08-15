//! Process that writes `segments/%06d.mkv` (or a test double).

use reelforge_capture_core::{CaptureError, CaptureSpec, Result};
use reelforge_capture_platform::{FfmpegGrab, grab_command, spawn_grab};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Host (or fake) that records into a session directory.
pub trait Grabber {
    /// Start writing from `segment_start` (1-based).
    ///
    /// # Errors
    ///
    /// Spawn / host failure.
    fn spawn(&mut self, spec: &CaptureSpec, session_dir: &Path, segment_start: u32) -> Result<()>;

    /// `Some(code)` once the process has exited. `None` while still running.
    ///
    /// # Errors
    ///
    /// Wait / I/O.
    fn poll_exit(&mut self) -> Result<Option<i32>>;

    /// Ask for a clean stop (`q` on ffmpeg stdin).
    ///
    /// # Errors
    ///
    /// Write failure.
    fn request_stop(&mut self) -> Result<()>;

    /// Force-kill. Idempotent.
    ///
    /// # Errors
    ///
    /// Kill / wait failure.
    fn kill(&mut self) -> Result<()>;

    /// OS pid while running.
    fn pid(&self) -> Option<u32> {
        None
    }

    /// Recent stderr (ffmpeg device / mux errors).
    fn stderr_tail(&self) -> String {
        String::new()
    }
}

/// Real host ffmpeg grab.
#[derive(Debug, Default)]
pub struct FfmpegGrabber {
    child: Option<Child>,
    pid: Option<u32>,
    stderr: Arc<Mutex<Vec<String>>>,
    stderr_thread: Option<JoinHandle<()>>,
}

impl FfmpegGrabber {
    /// Empty handle (not yet spawned).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn take_child(&mut self) -> Option<Child> {
        self.child.take()
    }
}

impl Grabber for FfmpegGrabber {
    fn spawn(&mut self, spec: &CaptureSpec, session_dir: &Path, segment_start: u32) -> Result<()> {
        if self.child.is_some() {
            self.kill()?;
        }
        let mut grab = grab_command(spec, session_dir)?;
        grab.set_segment_start(segment_start);
        let mut child = spawn_grab(&grab)?;
        self.pid = Some(child.id());
        self.stderr = Arc::new(Mutex::new(Vec::new()));
        if let Some(pipe) = child.stderr.take() {
            let buf = Arc::clone(&self.stderr);
            self.stderr_thread = Some(std::thread::spawn(move || {
                let reader = BufReader::new(pipe);
                for line in reader.lines().map_while(std::result::Result::ok) {
                    if let Ok(mut lines) = buf.lock() {
                        if lines.len() >= 40 {
                            lines.remove(0);
                        }
                        lines.push(line);
                    }
                }
            }));
        }
        self.child = Some(child);
        Ok(())
    }

    fn poll_exit(&mut self) -> Result<Option<i32>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(Some(0));
        };
        match child
            .try_wait()
            .map_err(|e| CaptureError::io(format!("ffmpeg wait: {e}")))?
        {
            Some(status) => {
                self.child = None;
                self.pid = None;
                Ok(Some(status.code().unwrap_or(-1)))
            }
            None => Ok(None),
        }
    }

    fn request_stop(&mut self) -> Result<()> {
        if let Some(child) = self.child.as_mut() {
            FfmpegGrab::request_quit(child)?;
        }
        Ok(())
    }

    fn kill(&mut self) -> Result<()> {
        if let Some(mut child) = self.take_child() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.pid = None;
        Ok(())
    }

    fn pid(&self) -> Option<u32> {
        self.pid
    }

    fn stderr_tail(&self) -> String {
        self.stderr
            .lock()
            .map(|lines| lines.join("\n"))
            .unwrap_or_default()
    }
}

impl Drop for FfmpegGrabber {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

/// In-process grabber for supervisor tests (no ffmpeg).
#[derive(Debug, Clone, Default)]
pub struct FakeGrabber {
    /// How many times [`Grabber::spawn`] ran.
    pub spawned: u32,
    /// Last `segment_start` passed to [`Grabber::spawn`].
    pub last_segment_start: u32,
    /// Whether a process is considered alive.
    pub running: bool,
    /// When set, [`Grabber::poll_exit`] returns it (and clears `running`).
    pub exit_code: Option<i32>,
    /// Set by [`Grabber::request_stop`].
    pub stop_requested: bool,
}

impl Grabber for FakeGrabber {
    fn spawn(
        &mut self,
        _spec: &CaptureSpec,
        _session_dir: &Path,
        segment_start: u32,
    ) -> Result<()> {
        self.spawned = self.spawned.saturating_add(1);
        self.last_segment_start = segment_start.max(1);
        self.running = true;
        self.exit_code = None;
        self.stop_requested = false;
        Ok(())
    }

    fn poll_exit(&mut self) -> Result<Option<i32>> {
        if let Some(code) = self.exit_code {
            self.running = false;
            return Ok(Some(code));
        }
        Ok(None)
    }

    fn request_stop(&mut self) -> Result<()> {
        self.stop_requested = true;
        self.running = false;
        self.exit_code = Some(0);
        Ok(())
    }

    fn kill(&mut self) -> Result<()> {
        self.running = false;
        if self.exit_code.is_none() {
            self.exit_code = Some(1);
        }
        Ok(())
    }
}

/// Poll until exit or timeout. `None` = still running.
///
/// # Errors
///
/// `poll_exit`.
pub(crate) fn wait_exit(grabber: &mut impl Grabber, timeout: Duration) -> Result<Option<i32>> {
    let start = Instant::now();
    loop {
        if let Some(code) = grabber.poll_exit()? {
            return Ok(Some(code));
        }
        if start.elapsed() >= timeout {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
