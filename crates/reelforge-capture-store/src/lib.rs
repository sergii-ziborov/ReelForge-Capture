//! Crash-safe session directory: manifest + WAL + closed segments + event log.

mod audio;
mod clocks;
mod control;
mod waveform;

pub use audio::{AUDIO_SIDECAR_VERSION, AudioLegTrack, AudioSegmentFile, AudioSidecar};
pub use clocks::{CLOCK_SIDECAR_VERSION, ClockAudioLeg, ClockMaster, ClockSegment, ClockSidecar};
pub use control::ControlOp;
pub use waveform::{WAVEFORM_SIDECAR_VERSION, WaveformLeg, WaveformPeak, WaveformSidecar};

use reelforge_capture_core::{
    CaptureError, CaptureSpec, HZ_1K, MediaTime, PointerEvent, Result, SegmentId, SessionId,
    SessionMeta,
};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// On-disk manifest (committed state only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionManifest {
    /// Schema.
    pub version: u32,
    /// Header.
    pub meta: SessionMeta,
    /// Closed segments in order.
    #[serde(default)]
    pub segments: Vec<SegmentRecord>,
}

/// One finalized media file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentRecord {
    /// Ordinal.
    pub id: SegmentId,
    /// Relative path from the session root.
    pub path: String,
    /// Start on the session clock.
    pub start: MediaTime,
    /// End on the session clock.
    pub end: MediaTime,
}

/// WAL line (not committed until `commit_segment` / `checkpoint`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WalOp {
    /// Session created.
    Open {
        /// Header.
        meta: SessionMeta,
    },
    /// A segment file was opened (may be incomplete after crash).
    BeginSegment {
        /// Ordinal.
        id: SegmentId,
        /// Relative path.
        path: String,
        /// Start time.
        start: MediaTime,
    },
    /// Segment closed and is safe to keep.
    CommitSegment {
        /// Record.
        segment: SegmentRecord,
    },
    /// Manifest rewritten; WAL may be truncated after this.
    Checkpoint,
}

/// Open session on disk.
pub struct SessionStore {
    root: PathBuf,
    manifest: SessionManifest,
}

impl SessionStore {
    /// Create `root/<id>/` and write the first WAL + manifest.
    ///
    /// The session directory is created exclusively. If `root/<id>` already
    /// exists, this returns an error containing `already exists` and does not
    /// modify anything inside it.
    ///
    /// # Errors
    ///
    /// I/O, or the session directory already exists.
    pub fn create(root: impl AsRef<Path>, meta: SessionMeta) -> Result<Self> {
        let parent = root.as_ref();
        let dir = parent.join(meta.id.as_str());
        if dir.try_exists()? {
            return Err(session_exists(&dir));
        }
        fs::create_dir_all(parent)?;
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(session_exists(&dir));
            }
            Err(e) => return Err(e.into()),
        }
        fs::create_dir(dir.join("segments"))?;
        let store = Self {
            root: dir,
            manifest: SessionManifest {
                version: 1,
                meta: meta.clone(),
                segments: Vec::new(),
            },
        };
        store.append_wal(&WalOp::Open { meta })?;
        store.write_manifest()?;
        File::create(store.events_path())?;
        Ok(store)
    }

    /// Open an existing session and replay the WAL (drops an unfinished tail).
    ///
    /// # Errors
    ///
    /// Missing dir / I/O / JSON.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let root = dir.as_ref().to_path_buf();
        if !root.is_dir() {
            return Err(CaptureError::message(format!(
                "session dir missing: {}",
                root.display()
            )));
        }
        let mut store = Self {
            root,
            manifest: SessionManifest {
                version: 1,
                meta: SessionMeta {
                    id: SessionId::new("unknown"),
                    name: String::new(),
                    spec: CaptureSpec::screen(),
                    started_unix: None,
                    duration: None,
                },
                segments: Vec::new(),
            },
        };
        store.replay()?;
        store.write_manifest()?;
        store.truncate_wal_after_checkpoint()?;
        Ok(store)
    }

    /// Session directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Committed manifest.
    #[must_use]
    pub fn manifest(&self) -> &SessionManifest {
        &self.manifest
    }

    /// Relative path for the next segment file.
    #[must_use]
    pub fn next_segment_rel(&self) -> String {
        let id = self
            .manifest
            .segments
            .last()
            .map_or(SegmentId::first(), |s| s.id.next());
        format!("segments/{}.mkv", id.file_stem())
    }

    /// Record that a segment file is being written.
    ///
    /// # Errors
    ///
    /// I/O.
    pub fn begin_segment(
        &mut self,
        id: SegmentId,
        rel: impl Into<String>,
        start: MediaTime,
    ) -> Result<()> {
        self.append_wal(&WalOp::BeginSegment {
            id,
            path: rel.into(),
            start,
        })
    }

    /// Close a segment (media file must already be durable).
    ///
    /// # Errors
    ///
    /// I/O.
    pub fn commit_segment(&mut self, segment: SegmentRecord) -> Result<()> {
        self.manifest.meta.duration = Some(segment.end);
        self.manifest.segments.push(segment.clone());
        self.append_wal(&WalOp::CommitSegment { segment })?;
        self.write_manifest()
    }

    /// Append a pointer event (crash-safe: one JSON line + flush).
    ///
    /// # Errors
    ///
    /// I/O.
    pub fn append_event(&self, event: &PointerEvent) -> Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.events_path())?;
        writeln!(f, "{}", serde_json::to_string(event)?)?;
        f.flush()?;
        Ok(())
    }

    /// Load the event log.
    ///
    /// # Errors
    ///
    /// I/O / JSON.
    pub fn load_events(&self) -> Result<Vec<PointerEvent>> {
        let path = self.events_path();
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let f = File::open(path)?;
        let mut out = Vec::new();
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            out.push(serde_json::from_str(&line)?);
        }
        Ok(out)
    }

    /// Closed duration on the 1 kHz clock (zero if empty).
    #[must_use]
    pub fn closed_duration(&self) -> MediaTime {
        self.manifest
            .meta
            .duration
            .unwrap_or_else(|| MediaTime::zero(HZ_1K))
    }

    /// Where `audio.json` lives.
    #[must_use]
    pub fn audio_sidecar_path(&self) -> PathBuf {
        self.root.join("audio.json")
    }

    /// Read `audio.json` (`None` when audio was never materialized).
    ///
    /// # Errors
    ///
    /// I/O / JSON.
    pub fn read_audio_sidecar(&self) -> Result<Option<AudioSidecar>> {
        let path = self.audio_sidecar_path();
        if !path.is_file() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&fs::read_to_string(path)?)?))
    }

    /// Write `audio.json` atomically.
    ///
    /// # Errors
    ///
    /// I/O / JSON.
    pub fn write_audio_sidecar(&self, sidecar: &AudioSidecar) -> Result<()> {
        let tmp = self.root.join("audio.json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(sidecar)?)?;
        fs::rename(tmp, self.audio_sidecar_path())?;
        Ok(())
    }

    fn events_path(&self) -> PathBuf {
        self.root.join("events.jsonl")
    }

    fn wal_path(&self) -> PathBuf {
        self.root.join("wal.jsonl")
    }

    fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    fn append_wal(&self, op: &WalOp) -> Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.wal_path())?;
        writeln!(f, "{}", serde_json::to_string(op)?)?;
        f.flush()?;
        Ok(())
    }

    fn write_manifest(&self) -> Result<()> {
        let tmp = self.root.join("manifest.json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&self.manifest)?)?;
        fs::rename(tmp, self.manifest_path())?;
        Ok(())
    }

    fn replay(&mut self) -> Result<()> {
        let wal = self.wal_path();
        if !wal.is_file() {
            if self.manifest_path().is_file() {
                self.manifest = serde_json::from_str(&fs::read_to_string(self.manifest_path())?)?;
            }
            return Ok(());
        }
        let lines = BufReader::new(File::open(&wal)?)
            .lines()
            .collect::<std::io::Result<Vec<_>>>()?;
        let last = lines.len().saturating_sub(1);
        let mut open: Option<WalOp> = None;
        let mut committed = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            // A crash can tear the final line. Only that tail is dropped.
            let op = match serde_json::from_str::<WalOp>(line) {
                Ok(op) => op,
                Err(_) if i == last => continue,
                Err(e) => return Err(e.into()),
            };
            match op {
                WalOp::Open { meta } => {
                    open = Some(WalOp::Open { meta });
                    committed.clear();
                }
                WalOp::BeginSegment { .. } | WalOp::Checkpoint => {
                    // Unfinished tail or checkpoint: keep committed segments only.
                }
                WalOp::CommitSegment { segment } => committed.push(segment),
            }
        }
        if let Some(WalOp::Open { meta }) = open {
            self.manifest.meta = meta;
        } else if self.manifest_path().is_file() {
            self.manifest = serde_json::from_str(&fs::read_to_string(self.manifest_path())?)?;
            return Ok(());
        }
        self.manifest.segments = committed;
        self.manifest.meta.duration = self.manifest.segments.last().map(|s| s.end);
        Ok(())
    }

    fn truncate_wal_after_checkpoint(&self) -> Result<()> {
        self.append_wal(&WalOp::Checkpoint)?;
        let compact = WalOp::Open {
            meta: self.manifest.meta.clone(),
        };
        let mut lines = vec![serde_json::to_string(&compact)?];
        for s in &self.manifest.segments {
            lines.push(serde_json::to_string(&WalOp::CommitSegment {
                segment: s.clone(),
            })?);
        }
        lines.push(serde_json::to_string(&WalOp::Checkpoint)?);
        let tmp = self.root.join("wal.jsonl.tmp");
        fs::write(&tmp, lines.join("\n") + "\n")?;
        fs::rename(tmp, self.wal_path())?;
        Ok(())
    }
}

fn session_exists(dir: &Path) -> CaptureError {
    CaptureError::message(format!("session already exists: {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::SessionStore;
    use crate::SegmentRecord;
    use reelforge_capture_core::{
        CaptureError, CaptureSpec, ClickButton, HZ_1K, MediaTime, PointerEvent, SegmentId,
        SessionId, SessionMeta,
    };
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_root() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rf-store-{n}"))
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

    fn read(path: &Path) -> Vec<u8> {
        fs::read(path).unwrap()
    }

    #[test]
    fn create_refuses_an_existing_session_without_writing() {
        let root = tmp_root();
        let mut s = SessionStore::create(&root, meta("ses_a")).unwrap();
        let end = MediaTime::from_secs(5.0, HZ_1K).unwrap();
        s.commit_segment(SegmentRecord {
            id: SegmentId::first(),
            path: "segments/000001.mkv".into(),
            start: MediaTime::zero(HZ_1K),
            end,
        })
        .unwrap();
        s.append_event(&PointerEvent::Click {
            t: MediaTime::from_secs(0.2, HZ_1K).unwrap(),
            x: 3,
            y: 4,
            button: ClickButton::Left,
        })
        .unwrap();
        let segments_before = s.manifest().segments.clone();
        let events_before = s.load_events().unwrap();
        let session = s.root().to_path_buf();
        drop(s);

        let manifest = read(&session.join("manifest.json"));
        let wal = read(&session.join("wal.jsonl"));
        let events = read(&session.join("events.jsonl"));
        let Err(err) = SessionStore::create(&root, meta("ses_a")) else {
            panic!("second create must fail");
        };
        match err {
            CaptureError::Message(m) => assert!(m.contains("already exists"), "{m}"),
            other => panic!("expected message, got {other}"),
        }
        assert_eq!(manifest, read(&session.join("manifest.json")));
        assert_eq!(wal, read(&session.join("wal.jsonl")));
        assert_eq!(events, read(&session.join("events.jsonl")));

        let opened = SessionStore::open(&session).unwrap();
        assert_eq!(opened.manifest().segments, segments_before);
        assert_eq!(opened.load_events().unwrap(), events_before);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn truncated_wal_tail_is_dropped() {
        let root = tmp_root();
        let mut s = SessionStore::create(&root, meta("ses_wal")).unwrap();
        let end = MediaTime::from_secs(5.0, HZ_1K).unwrap();
        s.commit_segment(SegmentRecord {
            id: SegmentId::first(),
            path: "segments/000001.mkv".into(),
            start: MediaTime::zero(HZ_1K),
            end,
        })
        .unwrap();
        let session = s.root().to_path_buf();
        drop(s);

        let mut wal = OpenOptions::new()
            .append(true)
            .open(session.join("wal.jsonl"))
            .unwrap();
        writeln!(wal, "{{").unwrap();
        drop(wal);

        let opened = SessionStore::open(&session).unwrap();
        assert_eq!(opened.manifest().segments.len(), 1);
        assert_eq!(opened.manifest().segments[0].end, end);
        assert_eq!(opened.manifest().segments[0].path, "segments/000001.mkv");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_wal_line_before_the_tail_is_an_error() {
        let root = tmp_root();
        let mut s = SessionStore::create(&root, meta("ses_bad")).unwrap();
        s.commit_segment(SegmentRecord {
            id: SegmentId::first(),
            path: "segments/000001.mkv".into(),
            start: MediaTime::zero(HZ_1K),
            end: MediaTime::from_secs(5.0, HZ_1K).unwrap(),
        })
        .unwrap();
        let session = s.root().to_path_buf();
        drop(s);

        let wal = session.join("wal.jsonl");
        let text = fs::read_to_string(&wal).unwrap();
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        assert!(lines.len() >= 2, "{text}");
        lines.insert(lines.len() - 1, "{".into());
        fs::write(&wal, lines.join("\n") + "\n").unwrap();
        let Err(err) = SessionStore::open(&session) else {
            panic!("a corrupt line before the tail must fail open");
        };
        assert!(err.to_string().contains("json"), "{err}");
        let _ = fs::remove_dir_all(root);
    }
}
