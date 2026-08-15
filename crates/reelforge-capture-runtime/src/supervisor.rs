//! Owns a live session: store + grabber + clock + segment harvest.

use crate::grabber::{FfmpegGrabber, Grabber, wait_exit};
use reelforge_capture_core::{
    CaptureError, CaptureSpec, ClickButton, HZ_1K, MediaTime, PointerEvent, Result, SegmentId,
    SessionId, SessionMeta,
};
use reelforge_capture_platform::{
    HostPointer, PointerSample, PointerSource, available_bytes, probe_audio_duration,
    probe_duration,
};
use reelforge_capture_store::{SegmentRecord, SessionStore};
use std::fmt;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const STOP_WAIT: Duration = Duration::from_secs(5);
const DISK_FLOOR: u64 = 8 * 1024 * 1024;
const GAP_SECS: f64 = 0.75;
const CURSOR_HEARTBEAT: i64 = 250;

/// Lifecycle of [`SessionSupervisor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        };
        sup.write_pid();
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
        })
    }

    /// Drive harvest + process watch. Call from a loop (~100–200 ms).
    ///
    /// # Errors
    ///
    /// Store I/O. Process death is reported as [`SupervisorEvent::Failed`], not an error.
    pub fn tick(&mut self) -> Result<Vec<SupervisorEvent>> {
        if self.phase != SessionPhase::Recording {
            return Ok(Vec::new());
        }
        if let Some(ev) = self.check_disk() {
            return Ok(vec![ev]);
        }
        self.sample_pointer()?;
        if let Some(code) = self.grabber.poll_exit()? {
            return self.on_process_exit(code);
        }
        self.harvest(Harvest::Prefix)
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
            self.clock.pause();
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
        let commit_upto = match mode {
            Harvest::Prefix if files.len() <= 1 => 0,
            Harvest::Prefix => files.len() - 1,
            Harvest::All => files.len(),
        };
        let mut evs = Vec::new();
        for file in files.iter().take(commit_upto) {
            if file.bytes == 0 || self.is_committed(&file.rel) {
                continue;
            }
            evs.extend(self.commit_file(file)?);
        }
        if matches!(mode, Harvest::Prefix)
            && let Some(newest) = files.last()
            && newest.bytes > 0
            && !self.is_committed(&newest.rel)
            && self.open.as_ref().map(|(id, _)| *id) != Some(newest.id)
        {
            let start = self.store.closed_duration();
            self.store.begin_segment(newest.id, &newest.rel, start)?;
            self.open = Some((newest.id, newest.rel.clone()));
            evs.push(SupervisorEvent::SegmentOpened {
                id: newest.id,
                path: newest.rel.clone(),
            });
        }
        Ok(evs)
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
        let probed = probe_duration(self.store.root().join(&file.rel))
            .ok()
            .flatten();
        let end = if let Some(dur) = probed {
            MediaTime {
                ticks: start.ticks.saturating_add(dur.ticks),
                timescale: HZ_1K,
            }
        } else if now.ticks <= start.ticks {
            MediaTime {
                ticks: start.ticks.saturating_add(1),
                timescale: HZ_1K,
            }
        } else {
            now
        };
        let expected = if now.ticks > start.ticks {
            now.as_secs() - start.as_secs()
        } else {
            self.spec.segment_secs
        };
        let actual = end.as_secs() - start.as_secs();
        if (actual - expected).abs() > GAP_SECS {
            let _ = self.store.append_event(&PointerEvent::FrameGap {
                t: now,
                expected_secs: expected,
                actual_secs: actual,
            });
        }
        if let Ok(Some(audio)) = probe_audio_duration(self.store.root().join(&file.rel))
            && (audio.as_secs() - actual).abs() > GAP_SECS
        {
            let _ = self.store.append_event(&PointerEvent::AudioGap {
                t: now,
                expected_secs: actual,
                actual_secs: audio.as_secs(),
            });
        }
        let segment = SegmentRecord {
            id: file.id,
            path: file.rel.clone(),
            start,
            end,
        };
        self.store.commit_segment(segment.clone())?;
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
        self.emit_click_edge(now, sample)?;
        self.last_pointer = Some(sample);
        Ok(())
    }

    fn emit_click_edge(&self, t: MediaTime, sample: PointerSample) -> Result<()> {
        let prev = self.last_pointer;
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
    fn unexpected_exit_does_not_commit_tail() {
        let (root, mut sup) = start("ses_die");
        touch(sup.store(), "000001.mkv");
        touch(sup.store(), "000002.mkv");
        let _ = sup.tick().unwrap();
        sup.grabber.exit_code = Some(1);
        let evs = sup.tick().unwrap();
        assert!(
            evs.iter()
                .any(|e| matches!(e, SupervisorEvent::Failed { .. }))
        );
        assert_eq!(sup.phase(), SessionPhase::Failed);
        assert_eq!(sup.store().manifest().segments.len(), 1);
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
                    left: false,
                    right: false,
                    middle: false,
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
            right: false,
            middle: false,
        });
        let _ = sup.tick().unwrap();
        let ev = sup.store().load_events().unwrap();
        assert!(ev.iter().any(|e| e.is_pointer() && !e.is_click()));
        assert!(ev.iter().any(PointerEvent::is_click));
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
