//! Owns a live session: store + grabber + clock + segment harvest.

use crate::clock::{ClockSample, clock_row, decide_clocks};
use crate::grabber::{FfmpegGrabber, Grabber, wait_exit};
use reelforge_capture_core::{
    CaptureError, CaptureSpec, ClickButton, HZ_1K, MediaTime, PointerEvent, Result, SegmentId,
    SessionId, SessionMeta,
};
use reelforge_capture_platform::{
    HostPointer, PointerSample, PointerSource, available_bytes, probe_media_clocks,
};
use reelforge_capture_store::{ControlOp, SegmentRecord, SessionStore};
use std::fmt;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const STOP_WAIT: Duration = Duration::from_secs(5);
const DISK_FLOOR: u64 = 8 * 1024 * 1024;
const GAP_SECS: f64 = 0.75;
const CURSOR_HEARTBEAT: i64 = 250;
const RESTART_WINDOW: Duration = Duration::from_secs(30);
const RESTART_LIMIT: usize = 3;

/// Lifecycle of [`SessionSupervisor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    /// Store open; grabber not running (after recover, before resume).
    Idle,
    /// Grabber running; closed prefix is committed on each [`SessionSupervisor::tick`].
    Recording,
    /// Grabber stopped cleanly; clock frozen; store kept.
    Paused,
    /// Clean stop; this handle will not record again.
    Stopped,
    /// Grabber died or harvest failed. Tail was *not* committed.
    Failed,
}

/// On-disk `status.json` for another process to poll.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LiveStatus {
    /// Current phase.
    pub phase: SessionPhase,
    /// Session id.
    pub id: String,
    /// ffmpeg pid, if running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Elapsed recording milliseconds.
    pub elapsed_ms: i64,
    /// Committed segment count.
    pub committed_segments: usize,
    /// Closed duration in milliseconds (1 kHz ticks).
    pub closed_duration_ms: i64,
    /// Last failure, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Snapshot for CLI / desktop.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionStatus {
    /// Current phase.
    pub phase: SessionPhase,
    /// Session id.
    pub id: SessionId,
    /// Elapsed recording time (pauses excluded).
    pub elapsed: MediaTime,
    /// Committed segment count.
    pub committed_segments: usize,
    /// Closed duration on the session clock.
    pub closed_duration: MediaTime,
    /// Last failure, if any.
    pub last_error: Option<String>,
}

/// Something the supervisor did during `tick` / `pause` / `stop`.
#[derive(Debug, Clone, PartialEq)]
pub enum SupervisorEvent {
    /// Grabber spawned.
    Started,
    /// WAL `begin_segment` for a file now on disk.
    SegmentOpened {
        /// Ordinal.
        id: SegmentId,
        /// Relative path.
        path: String,
    },
    /// WAL `commit_segment` — file is durable.
    SegmentCommitted {
        /// Record written to the manifest.
        segment: SegmentRecord,
    },
    /// Recording paused.
    Paused,
    /// Recording resumed.
    Resumed,
    /// Clean stop.
    Stopped,
    /// Grabber died; a new process was spawned from the next ordinal.
    Restarted {
        /// 1-based attempt inside the current window.
        attempt: u32,
        /// Exit code of the dead process.
        code: i32,
    },
    /// Process died or I/O failed; last file dropped.
    Failed {
        /// Why.
        reason: String,
    },
}

impl fmt::Display for SupervisorEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Started => write!(f, "started"),
            Self::SegmentOpened { id, path } => write!(f, "opened {} ({path})", id.0),
            Self::SegmentCommitted { segment } => write!(
                f,
                "committed {} through {:.3}s",
                segment.id.0,
                segment.end.as_secs()
            ),
            Self::Paused => write!(f, "paused"),
            Self::Resumed => write!(f, "resumed"),
            Self::Stopped => write!(f, "stopped"),
            Self::Restarted { attempt, code } => {
                write!(f, "restarted after exit {code} (attempt {attempt})")
            }
            Self::Failed { reason } => write!(f, "failed: {reason}"),
        }
    }
}

struct SessionClock {
    accumulated_ticks: i64,
    running_since: Option<Instant>,
}

impl SessionClock {
    fn start() -> Self {
        Self {
            accumulated_ticks: 0,
            running_since: Some(Instant::now()),
        }
    }

    fn from_committed(duration: MediaTime) -> Self {
        Self {
            accumulated_ticks: duration.ticks.max(0),
            running_since: None,
        }
    }

    fn now(&self) -> MediaTime {
        let extra = self.running_since.map_or(0, elapsed_ms);
        MediaTime {
            ticks: self.accumulated_ticks.saturating_add(extra),
            timescale: HZ_1K,
        }
    }

    fn pause(&mut self) {
        if let Some(t) = self.running_since.take() {
            self.accumulated_ticks = self.accumulated_ticks.saturating_add(elapsed_ms(t));
        }
    }

    fn resume(&mut self) {
        if self.running_since.is_none() {
            self.running_since = Some(Instant::now());
        }
    }

    /// Snap the clock to media time (probed video out-point).
    ///
    /// Drops wall-clock lead so later pointer events stay on the same
    /// timeline as committed segments.
    fn slew_to(&mut self, media: MediaTime) {
        self.accumulated_ticks = media.ticks.max(0);
        if self.running_since.is_some() {
            self.running_since = Some(Instant::now());
        }
    }
}

fn elapsed_ms(start: Instant) -> i64 {
    i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

#[derive(Clone, Copy)]
enum Harvest {
    /// All files except the newest (ffmpeg is still writing it).
    Prefix,
    /// Every non-empty file (clean stop / pause).
    All,
}

struct SegFile {
    id: SegmentId,
    rel: String,
    bytes: u64,
}

/// Owns the live capture: store, grabber, session clock, WAL harvest.
pub struct SessionSupervisor<G: Grabber = FfmpegGrabber, P: PointerSource = HostPointer> {
    store: SessionStore,
    grabber: G,
    pointer: P,
    spec: CaptureSpec,
    clock: SessionClock,
    phase: SessionPhase,
    open: Option<(SegmentId, String)>,
    last_error: Option<String>,
    stop_requested: bool,
    last_pointer: Option<PointerSample>,
    last_cursor_ticks: i64,
    restarts: Vec<Instant>,
}

impl SessionSupervisor<FfmpegGrabber, HostPointer> {
    /// Create a new session directory and start recording.
    ///
    /// # Errors
    ///
    /// Store I/O or grabber spawn.
    pub fn start(root: impl AsRef<Path>, meta: SessionMeta) -> Result<Self> {
        Self::start_with(root, meta, FfmpegGrabber::new(), HostPointer::default())
    }

    /// Open an existing session (WAL replay, unfinished tail dropped). Grabber stays idle.
    ///
    /// # Errors
    ///
    /// Missing dir / I/O.
    pub fn recover(dir: impl AsRef<Path>) -> Result<Self> {
        Self::recover_with(dir, FfmpegGrabber::new(), HostPointer::default())
    }
}

impl<G: Grabber, P: PointerSource> SessionSupervisor<G, P> {
    /// [`Self::start`] with injected grabber / pointer (tests).
    ///
    /// # Errors
    ///
    /// Store I/O or grabber spawn.
    pub fn start_with(
        root: impl AsRef<Path>,
        mut meta: SessionMeta,
        mut grabber: G,
        pointer: P,
    ) -> Result<Self> {
        if meta.started_unix.is_none() {
            meta.started_unix = Some(unix_now());
        }
        let spec = meta.spec.clone();
        let store = SessionStore::create(root, meta)?;
        grabber.spawn(&spec, store.root(), 1)?;
        let sup = Self {
            store,
            grabber,
            pointer,
            spec,
            clock: SessionClock::start(),
            phase: SessionPhase::Recording,
            open: None,
            last_error: None,
            stop_requested: false,
            last_pointer: None,
            last_cursor_ticks: i64::MIN,
            restarts: Vec::new(),
        };
        sup.write_pid();
        let _ = sup.write_status();
        Ok(sup)
    }

    /// [`Self::recover`] with injected I/O.
    ///
    /// # Errors
    ///
    /// Missing dir / I/O.
    pub fn recover_with(dir: impl AsRef<Path>, grabber: G, pointer: P) -> Result<Self> {
        let store = SessionStore::open(dir)?;
        let spec = store.manifest().meta.spec.clone();
        let clock = SessionClock::from_committed(store.closed_duration());
        Ok(Self {
            store,
            grabber,
            pointer,
            spec,
            clock,
            phase: SessionPhase::Idle,
            open: None,
            last_error: None,
            stop_requested: false,
            last_pointer: None,
            last_cursor_ticks: i64::MIN,
            restarts: Vec::new(),
        })
    }

    /// Drive harvest + process watch. Call from a loop (~100–200 ms).
    ///
    /// # Errors
    ///
    /// Store I/O. Process death is reported as [`SupervisorEvent::Failed`], not an error.
    pub fn tick(&mut self) -> Result<Vec<SupervisorEvent>> {
        let mut evs = self.drain_control()?;
        let _ = self.write_status();
        if self.phase != SessionPhase::Recording {
            return Ok(evs);
        }
        if let Some(ev) = self.check_disk() {
            evs.push(ev);
            return Ok(evs);
        }
        self.sample_pointer()?;
        if let Some(code) = self.grabber.poll_exit()? {
            evs.extend(self.on_process_exit(code)?);
            return Ok(evs);
        }
        evs.extend(self.harvest(Harvest::Prefix)?);
        Ok(evs)
    }

    /// Freeze the clock and stop the grabber (last closed file is committed).
    ///
    /// # Errors
    ///
    /// Not recording, or store I/O.
    pub fn pause(&mut self) -> Result<Vec<SupervisorEvent>> {
        if self.phase != SessionPhase::Recording {
            return Err(CaptureError::message(format!(
                "pause requires recording (was {:?})",
                self.phase
            )));
        }
        let mut evs = self.stop_grabber(true)?;
        self.clock.pause();
        self.phase = SessionPhase::Paused;
        evs.push(SupervisorEvent::Paused);
        Ok(evs)
    }

    /// Spawn the grabber again from the next segment ordinal.
    ///
    /// # Errors
    ///
    /// Not idle/paused, or spawn failure.
    pub fn resume(&mut self) -> Result<Vec<SupervisorEvent>> {
        if !matches!(self.phase, SessionPhase::Idle | SessionPhase::Paused) {
            return Err(CaptureError::message(format!(
                "resume requires idle/paused (was {:?})",
                self.phase
            )));
        }
        let start = self.next_segment_number();
        self.grabber.spawn(&self.spec, self.store.root(), start)?;
        self.write_pid();
        self.clock.resume();
        self.phase = SessionPhase::Recording;
        self.stop_requested = false;
        self.last_error = None;
        Ok(vec![SupervisorEvent::Resumed])
    }

    /// Clean stop: finalize remaining files, freeze the clock.
    ///
    /// # Errors
    ///
    /// Store I/O.
    pub fn stop(&mut self) -> Result<SessionStatus> {
        if matches!(self.phase, SessionPhase::Stopped) {
            return Ok(self.status());
        }
        if self.phase == SessionPhase::Recording {
            let _ = self.stop_grabber(true)?;
        }
        self.clock.pause();
        self.phase = SessionPhase::Stopped;
        let _ = self.write_status();
        Ok(self.status())
    }

    /// Session clock (pauses excluded).
    #[must_use]
    pub fn now(&self) -> MediaTime {
        self.clock.now()
    }

    /// Current phase.
    #[must_use]
    pub const fn phase(&self) -> SessionPhase {
        self.phase
    }

    /// Status snapshot.
    #[must_use]
    pub fn status(&self) -> SessionStatus {
        SessionStatus {
            phase: self.phase,
            id: self.store.manifest().meta.id.clone(),
            elapsed: self.clock.now(),
            committed_segments: self.store.manifest().segments.len(),
            closed_duration: self.store.closed_duration(),
            last_error: self.last_error.clone(),
        }
    }

    /// Underlying store (edits / project emit).
    #[must_use]
    pub const fn store(&self) -> &SessionStore {
        &self.store
    }

    /// Append a cursor sample stamped with the session clock.
    ///
    /// # Errors
    ///
    /// I/O.
    pub fn push_cursor(&self, x: i32, y: i32) -> Result<()> {
        self.store.append_event(&PointerEvent::Cursor {
            t: self.clock.now(),
            x,
            y,
        })
    }

    /// Append a click stamped with the session clock.
    ///
    /// # Errors
    ///
    /// I/O.
    pub fn push_click(&self, x: i32, y: i32, button: ClickButton) -> Result<()> {
        self.store.append_event(&PointerEvent::Click {
            t: self.clock.now(),
            x,
            y,
            button,
        })
    }

    /// Append an already-stamped event.
    ///
    /// # Errors
    ///
    /// I/O.
    pub fn push_event(&self, event: &PointerEvent) -> Result<()> {
        self.store.append_event(event)
    }

    fn next_segment_number(&self) -> u32 {
        self.store
            .manifest()
            .segments
            .last()
            .map_or(1, |s| s.id.next().0)
    }

    fn is_committed(&self, rel: &str) -> bool {
        self.store.manifest().segments.iter().any(|s| s.path == rel)
    }

    fn on_process_exit(&mut self, code: i32) -> Result<Vec<SupervisorEvent>> {
        self.clear_pid();
        if self.stop_requested {
            let mut evs = self.harvest(Harvest::All)?;
            self.clock.pause();
            self.phase = SessionPhase::Stopped;
            evs.push(SupervisorEvent::Stopped);
            Ok(evs)
        } else {
            let mut evs = self.harvest(Harvest::Prefix)?;
            let mut reason = format!("grabber exited {code}");
            let tail = self.grabber.stderr_tail();
            if !tail.is_empty() {
                reason.push_str(": ");
                reason.push_str(&tail);
            }
            let _ = self.store.append_event(&PointerEvent::DeviceLost {
                t: self.clock.now(),
                detail: reason.clone(),
            });
            if self.try_restart()? {
                evs.push(SupervisorEvent::Restarted {
                    attempt: u32::try_from(self.restarts.len()).unwrap_or(u32::MAX),
                    code,
                });
                return Ok(evs);
            }
            self.clock.pause();
            self.last_error = Some(reason.clone());
            self.phase = SessionPhase::Failed;
            evs.push(SupervisorEvent::Failed { reason });
            Ok(evs)
        }
    }

    fn stop_grabber(&mut self, commit_all: bool) -> Result<Vec<SupervisorEvent>> {
        self.stop_requested = true;
        let _ = self.grabber.request_stop();
        if wait_exit(&mut self.grabber, STOP_WAIT)?.is_none() {
            self.grabber.kill()?;
        }
        let mode = if commit_all {
            Harvest::All
        } else {
            Harvest::Prefix
        };
        self.harvest(mode)
    }

    fn harvest(&mut self, mode: Harvest) -> Result<Vec<SupervisorEvent>> {
        let files = list_segments(self.store.root())?;
        let listed = listed_closed(self.store.root());
        let newest = files.last().map(|f| f.id);
        let mut evs = Vec::new();
        for file in &files {
            if file.bytes == 0 || self.is_committed(&file.rel) {
                continue;
            }
            let closed = match (&listed, mode) {
                (Some(set), _) => set.contains(&file.rel),
                (None, Harvest::Prefix) => Some(file.id) != newest,
                (None, Harvest::All) => true,
            };
            if closed {
                evs.extend(self.commit_file(file)?);
            }
        }
        if matches!(mode, Harvest::Prefix)
            && let Some(newest_file) = files.last()
            && newest_file.bytes > 0
            && !self.is_committed(&newest_file.rel)
            && listed
                .as_ref()
                .is_none_or(|set| !set.contains(&newest_file.rel))
            && self.open.as_ref().map(|(id, _)| *id) != Some(newest_file.id)
        {
            let start = self.store.closed_duration();
            self.store
                .begin_segment(newest_file.id, &newest_file.rel, start)?;
            self.open = Some((newest_file.id, newest_file.rel.clone()));
            evs.push(SupervisorEvent::SegmentOpened {
                id: newest_file.id,
                path: newest_file.rel.clone(),
            });
        }
        Ok(evs)
    }

    fn drain_control(&mut self) -> Result<Vec<SupervisorEvent>> {
        let Some(op) = self.store.take_control()? else {
            return Ok(Vec::new());
        };
        match op {
            ControlOp::Stop => {
                let _ = self.stop()?;
                Ok(vec![SupervisorEvent::Stopped])
            }
            ControlOp::Pause if self.phase == SessionPhase::Recording => self.pause(),
            ControlOp::Resume
                if matches!(self.phase, SessionPhase::Idle | SessionPhase::Paused) =>
            {
                self.resume()
            }
            ControlOp::Pause | ControlOp::Resume => Ok(Vec::new()),
        }
    }

    fn try_restart(&mut self) -> Result<bool> {
        let now = Instant::now();
        self.restarts
            .retain(|t| now.saturating_duration_since(*t) < RESTART_WINDOW);
        if self.restarts.len() >= RESTART_LIMIT {
            return Ok(false);
        }
        self.restarts.push(now);
        let start = self.next_segment_number();
        self.grabber.spawn(&self.spec, self.store.root(), start)?;
        self.write_pid();
        self.last_error = None;
        Ok(true)
    }

    fn write_status(&self) -> Result<()> {
        let st = self.status();
        let body = serde_json::to_string_pretty(&LiveStatus {
            phase: st.phase,
            id: st.id.as_str().to_string(),
            pid: self.grabber.pid(),
            elapsed_ms: st.elapsed.ticks,
            committed_segments: st.committed_segments,
            closed_duration_ms: st.closed_duration.ticks,
            last_error: st.last_error,
        })?;
        let dest = self.store.root().join("status.json");
        let tmp = self.store.root().join("status.json.tmp");
        fs::write(&tmp, body)?;
        // Windows cannot rename over an existing file.
        let _ = fs::remove_file(&dest);
        fs::rename(tmp, dest)?;
        Ok(())
    }

    fn commit_file(&mut self, file: &SegFile) -> Result<Vec<SupervisorEvent>> {
        let mut evs = Vec::new();
        let start = self.store.closed_duration();
        if self.open.as_ref().map(|(id, _)| *id) != Some(file.id) {
            self.store.begin_segment(file.id, &file.rel, start)?;
            evs.push(SupervisorEvent::SegmentOpened {
                id: file.id,
                path: file.rel.clone(),
            });
        }
        let now = self.clock.now();
        let path = self.store.root().join(&file.rel);
        let probed = probe_media_clocks(&path).unwrap_or_default();
        let decision = decide_clocks(
            start,
            &ClockSample::from_probed(now, &probed),
            self.spec.segment_secs,
            GAP_SECS,
        );
        if decision.frame_gap {
            let _ = self.store.append_event(&PointerEvent::FrameGap {
                t: now,
                expected_secs: if decision.session_secs > 0.0 {
                    decision.session_secs
                } else {
                    self.spec.segment_secs
                },
                actual_secs: decision.end.as_secs() - start.as_secs(),
            });
        }
        let master_secs = decision.end.as_secs() - start.as_secs();
        for leg in &decision.audio {
            let Some(actual) = leg.duration_secs else {
                continue;
            };
            if (actual - master_secs).abs() > GAP_SECS {
                let _ = self.store.append_event(&PointerEvent::AudioGap {
                    t: now,
                    expected_secs: master_secs,
                    actual_secs: actual,
                });
            }
        }
        let segment = SegmentRecord {
            id: file.id,
            path: file.rel.clone(),
            start,
            end: decision.end,
        };
        self.store.commit_segment(segment.clone())?;
        let _ = self
            .store
            .append_clock(clock_row(file.id, start, &decision));
        self.clock.slew_to(decision.end);
        self.open = None;
        evs.push(SupervisorEvent::SegmentCommitted { segment });
        Ok(evs)
    }

    fn write_pid(&self) {
        if let Some(pid) = self.grabber.pid() {
            let _ = fs::write(self.store.root().join("grab.pid"), pid.to_string());
        }
    }

    fn clear_pid(&self) {
        let _ = fs::remove_file(self.store.root().join("grab.pid"));
    }

    fn check_disk(&mut self) -> Option<SupervisorEvent> {
        let free = available_bytes(self.store.root())?;
        if free >= DISK_FLOOR {
            return None;
        }
        let _ = self.store.append_event(&PointerEvent::DiskFull {
            t: self.clock.now(),
        });
        let _ = self.stop_grabber(false);
        self.clock.pause();
        self.phase = SessionPhase::Failed;
        let reason = format!("disk full ({free} bytes free)");
        self.last_error = Some(reason.clone());
        Some(SupervisorEvent::Failed { reason })
    }

    fn sample_pointer(&mut self) -> Result<()> {
        if !self.spec.pointer {
            return Ok(());
        }
        let Some(sample) = self.pointer.sample() else {
            return Ok(());
        };
        let now = self.clock.now();
        let moved = self
            .last_pointer
            .as_ref()
            .is_none_or(|p| p.x != sample.x || p.y != sample.y);
        let heartbeat = now.ticks.saturating_sub(self.last_cursor_ticks) >= CURSOR_HEARTBEAT;
        if moved || heartbeat {
            self.store.append_event(&PointerEvent::Cursor {
                t: now,
                x: sample.x,
                y: sample.y,
            })?;
            self.last_cursor_ticks = now.ticks;
        }
        self.emit_click_edge(now, &sample)?;
        if sample.key && !self.last_pointer.as_ref().is_some_and(|p| p.key) {
            self.store.append_event(&PointerEvent::Key { t: now })?;
        }
        if sample.window != 0
            && self
                .last_pointer
                .as_ref()
                .is_none_or(|p| p.window != sample.window)
        {
            self.store.append_event(&PointerEvent::Window {
                t: now,
                hwnd: sample.window,
                title: sample.title.clone(),
            })?;
        }
        self.last_pointer = Some(sample);
        Ok(())
    }

    fn emit_click_edge(&self, t: MediaTime, sample: &PointerSample) -> Result<()> {
        let prev = self.last_pointer.as_ref();
        let edges = [
            (sample.left, prev.is_some_and(|p| p.left), ClickButton::Left),
            (
                sample.right,
                prev.is_some_and(|p| p.right),
                ClickButton::Right,
            ),
            (
                sample.middle,
                prev.is_some_and(|p| p.middle),
                ClickButton::Middle,
            ),
        ];
        for (down, was, button) in edges {
            if down && !was {
                self.store.append_event(&PointerEvent::Click {
                    t,
                    x: sample.x,
                    y: sample.y,
                    button,
                })?;
            }
        }
        Ok(())
    }
}

impl<G: Grabber, P: PointerSource> Drop for SessionSupervisor<G, P> {
    fn drop(&mut self) {
        if self.phase == SessionPhase::Recording {
            // Crash semantics: kill the grabber, do not commit the open tail.
            let _ = self.grabber.request_stop();
            let _ = self.grabber.kill();
        }
        self.clear_pid();
    }
}

fn listed_closed(root: &Path) -> Option<std::collections::HashSet<String>> {
    let path = root.join("closed.list");
    if !path.is_file() {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    let mut set = std::collections::HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(name) = Path::new(line).file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if parse_segment_name(name).is_some() {
            set.insert(format!("segments/{name}"));
        }
    }
    Some(set)
}

fn list_segments(root: &Path) -> Result<Vec<SegFile>> {
    let dir = root.join("segments");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id) = parse_segment_name(name) else {
            continue;
        };
        let bytes = entry.metadata()?.len();
        out.push(SegFile {
            id,
            rel: format!("segments/{name}"),
            bytes,
        });
    }
    out.sort_by_key(|s| s.id.0);
    Ok(out)
}

fn parse_segment_name(name: &str) -> Option<SegmentId> {
    let stem = name.strip_suffix(".mkv")?;
    let n: u32 = stem.parse().ok()?;
    (n >= 1).then_some(SegmentId(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FakeGrabber;
    use reelforge_capture_core::SessionId;
    use reelforge_capture_platform::{FakePointer, NullPointer};
    use std::path::PathBuf;

    fn tmp_root() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rf-sup-{n}"))
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

    fn touch(store: &SessionStore, name: &str) {
        fs::write(store.root().join("segments").join(name), b"mkv").unwrap();
    }

    fn start(id: &str) -> (PathBuf, SessionSupervisor<FakeGrabber, NullPointer>) {
        let root = tmp_root();
        let fake = FakeGrabber::default();
        let sup = SessionSupervisor::start_with(&root, meta(id), fake, NullPointer).unwrap();
        (root, sup)
    }

    #[test]
    fn start_records_and_commits_closed_prefix() {
        let (root, mut sup) = start("ses_prefix");
        assert_eq!(sup.phase(), SessionPhase::Recording);
        assert_eq!(sup.grabber.spawned, 1);
        assert!(sup.store().manifest().meta.started_unix.is_some());

        touch(sup.store(), "000001.mkv");
        let evs = sup.tick().unwrap();
        assert!(
            evs.iter()
                .any(|e| matches!(e, SupervisorEvent::SegmentOpened { id, .. } if id.0 == 1))
        );
        assert_eq!(sup.store().manifest().segments.len(), 0);

        touch(sup.store(), "000002.mkv");
        let evs = sup.tick().unwrap();
        assert!(evs.iter().any(|e| matches!(
            e,
            SupervisorEvent::SegmentCommitted { segment } if segment.id.0 == 1
        )));
        assert_eq!(sup.store().manifest().segments.len(), 1);
        assert_eq!(
            sup.store().manifest().segments[0].path,
            "segments/000001.mkv"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stop_commits_final_segment() {
        let (root, mut sup) = start("ses_stop");
        touch(sup.store(), "000001.mkv");
        let _ = sup.tick().unwrap();
        let st = sup.stop().unwrap();
        assert_eq!(st.phase, SessionPhase::Stopped);
        assert_eq!(st.committed_segments, 1);
        assert!(sup.grabber.stop_requested);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn crash_drop_leaves_open_tail() {
        let (root, mut sup) = start("ses_crash");
        let session = sup.store().root().to_path_buf();
        touch(sup.store(), "000001.mkv");
        touch(sup.store(), "000002.mkv");
        let _ = sup.tick().unwrap();
        assert_eq!(sup.store().manifest().segments.len(), 1);
        drop(sup);

        let opened = SessionSupervisor::<FakeGrabber, NullPointer>::recover_with(
            session,
            FakeGrabber::default(),
            NullPointer,
        )
        .unwrap();
        assert_eq!(opened.phase(), SessionPhase::Idle);
        assert_eq!(opened.store().manifest().segments.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unexpected_exit_restarts_and_keeps_the_tail_uncommitted() {
        let (root, mut sup) = start("ses_die");
        touch(sup.store(), "000001.mkv");
        touch(sup.store(), "000002.mkv");
        let _ = sup.tick().unwrap();
        sup.grabber.exit_code = Some(1);
        let evs = sup.tick().unwrap();
        assert!(
            evs.iter()
                .any(|e| matches!(e, SupervisorEvent::Restarted { code: 1, .. })),
            "{evs:?}"
        );
        assert_eq!(sup.phase(), SessionPhase::Recording);
        assert_eq!(sup.grabber.spawned, 2);
        assert_eq!(sup.store().manifest().segments.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn restart_budget_exhausted_fails() {
        let (root, mut sup) = start("ses_budget");
        for _ in 0..RESTART_LIMIT {
            sup.grabber.exit_code = Some(1);
            let evs = sup.tick().unwrap();
            assert!(
                evs.iter()
                    .any(|e| matches!(e, SupervisorEvent::Restarted { .. })),
                "{evs:?}"
            );
        }
        sup.grabber.exit_code = Some(1);
        let evs = sup.tick().unwrap();
        assert!(
            evs.iter()
                .any(|e| matches!(e, SupervisorEvent::Failed { .. })),
            "{evs:?}"
        );
        assert_eq!(sup.phase(), SessionPhase::Failed);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn closed_list_commits_even_the_newest_file() {
        let (root, mut sup) = start("ses_list");
        touch(sup.store(), "000001.mkv");
        fs::write(sup.store().root().join("closed.list"), "000001.mkv\n").unwrap();
        let evs = sup.tick().unwrap();
        assert!(evs.iter().any(|e| matches!(
            e,
            SupervisorEvent::SegmentCommitted { segment } if segment.id.0 == 1
        )));
        assert_eq!(sup.store().manifest().segments.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn control_stop_from_another_process() {
        let (root, mut sup) = start("ses_ipc");
        touch(sup.store(), "000001.mkv");
        sup.store().write_control(ControlOp::Stop).unwrap();
        let evs = sup.tick().unwrap();
        assert!(evs.iter().any(|e| matches!(e, SupervisorEvent::Stopped)));
        assert_eq!(sup.phase(), SessionPhase::Stopped);
        let text = fs::read_to_string(sup.store().root().join("status.json")).unwrap();
        let live: LiveStatus = serde_json::from_str(&text).unwrap();
        assert_eq!(live.phase, SessionPhase::Stopped);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn pause_freezes_clock_resume_continues_ordinals() {
        let (root, mut sup) = start("ses_pause");
        std::thread::sleep(Duration::from_millis(30));
        touch(sup.store(), "000001.mkv");
        let _ = sup.pause().unwrap();
        assert_eq!(sup.phase(), SessionPhase::Paused);
        let frozen = sup.now().ticks;
        assert!(frozen >= 30);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(sup.now().ticks, frozen);
        assert_eq!(sup.store().manifest().segments.len(), 1);

        let _ = sup.resume().unwrap();
        assert_eq!(sup.phase(), SessionPhase::Recording);
        assert_eq!(sup.grabber.spawned, 2);
        assert_eq!(sup.grabber.last_segment_start, 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn recover_then_resume_starts_next_ordinal() {
        let (root, mut sup) = start("ses_rec");
        let session = sup.store().root().to_path_buf();
        touch(sup.store(), "000001.mkv");
        let _ = sup.stop().unwrap();
        drop(sup);

        let mut opened =
            SessionSupervisor::recover_with(&session, FakeGrabber::default(), NullPointer).unwrap();
        assert_eq!(opened.store().manifest().segments.len(), 1);
        let _ = opened.resume().unwrap();
        assert_eq!(opened.grabber.last_segment_start, 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn push_cursor_uses_session_clock() {
        let (root, sup) = start("ses_ev");
        sup.push_cursor(10, 20).unwrap();
        let ev = sup.store().load_events().unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].position(), Some((10, 20)));
        assert!(!ev[0].is_click());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tick_records_pointer_and_click_edge() {
        let (root, mut sup) = {
            let root = tmp_root();
            let ptr = FakePointer {
                next: Some(PointerSample {
                    x: 4,
                    y: 8,
                    ..PointerSample::default()
                }),
            };
            let sup =
                SessionSupervisor::start_with(&root, meta("ses_ptr"), FakeGrabber::default(), ptr)
                    .unwrap();
            (root, sup)
        };
        let _ = sup.tick().unwrap();
        sup.pointer.next = Some(PointerSample {
            x: 4,
            y: 8,
            left: true,
            ..PointerSample::default()
        });
        let _ = sup.tick().unwrap();
        let ev = sup.store().load_events().unwrap();
        assert!(ev.iter().any(|e| e.is_pointer() && !e.is_click()));
        assert!(ev.iter().any(PointerEvent::is_click));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tick_records_key_and_window_change() {
        let (root, mut sup) = {
            let root = tmp_root();
            let ptr = FakePointer {
                next: Some(PointerSample {
                    window: 1,
                    title: "a".into(),
                    ..PointerSample::default()
                }),
            };
            let sup =
                SessionSupervisor::start_with(&root, meta("ses_kw"), FakeGrabber::default(), ptr)
                    .unwrap();
            (root, sup)
        };
        let _ = sup.tick().unwrap();
        sup.pointer.next = Some(PointerSample {
            key: true,
            window: 2,
            title: "b".into(),
            ..PointerSample::default()
        });
        let _ = sup.tick().unwrap();
        let ev = sup.store().load_events().unwrap();
        assert!(ev.iter().any(|e| matches!(e, PointerEvent::Key { .. })));
        assert_eq!(
            ev.iter()
                .filter(|e| matches!(e, PointerEvent::Window { .. }))
                .count(),
            2
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn clock_slews_to_media_and_drops_wall_lead() {
        let mut clock = SessionClock::start();
        std::thread::sleep(Duration::from_millis(40));
        assert!(clock.now().ticks >= 40);
        clock.slew_to(MediaTime {
            ticks: 5,
            timescale: HZ_1K,
        });
        let n = clock.now().ticks;
        assert!(n < 25, "slew should drop the 40 ms wall lead, got {n}");
    }

    #[test]
    fn commit_writes_clocks_sidecar() {
        let (root, mut sup) = start("ses_clk");
        touch(sup.store(), "000001.mkv");
        fs::write(sup.store().root().join("closed.list"), "000001.mkv\n").unwrap();
        let _ = sup.tick().unwrap();
        let clocks = sup.store().read_clocks().unwrap().expect("clocks.json");
        assert_eq!(clocks.segments.len(), 1);
        assert_eq!(clocks.segments[0].id, SegmentId(1));
        assert_eq!(
            clocks.segments[0].master,
            reelforge_capture_store::ClockMaster::Session
        );
        assert_eq!(
            clocks.segments[0].end,
            sup.store().manifest().segments[0].end
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn illegal_pause_when_idle() {
        let (root, mut sup) = start("ses_bad");
        let _ = sup.stop().unwrap();
        assert!(sup.pause().is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
