//! Crash-safe session directory: manifest + WAL + closed segments + event log.

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
    /// # Errors
    ///
    /// I/O.
    pub fn create(root: impl AsRef<Path>, meta: SessionMeta) -> Result<Self> {
        let dir = root.as_ref().join(meta.id.as_str());
        fs::create_dir_all(dir.join("segments"))?;
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
        let f = File::open(&wal)?;
        let mut open: Option<WalOp> = None;
        let mut committed = Vec::new();
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(&line)? {
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
