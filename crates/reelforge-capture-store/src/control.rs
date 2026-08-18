//! Out-of-process control of a live supervisor (`control.json`).
//!
//! Another process (CLI `stop` / `pause` / `resume`) writes one op; the
//! supervisor consumes it on the next `tick`. One outstanding command at a
//! time — a later write replaces an unread one.

use crate::SessionStore;
use reelforge_capture_core::Result;
use serde::{Deserialize, Serialize};
use std::fs;

/// Command for the running supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlOp {
    /// Clean stop.
    Stop,
    /// Freeze the clock and stop the grabber.
    Pause,
    /// Spawn the grabber again.
    Resume,
}

impl SessionStore {
    /// Path of the control file.
    #[must_use]
    pub fn control_path(&self) -> std::path::PathBuf {
        self.root.join("control.json")
    }

    /// Ask the live supervisor to do `op` (atomic replace).
    ///
    /// # Errors
    ///
    /// I/O / JSON.
    pub fn write_control(&self, op: ControlOp) -> Result<()> {
        let tmp = self.root.join("control.json.tmp");
        fs::write(&tmp, serde_json::to_string(&op)?)?;
        fs::rename(tmp, self.control_path())?;
        Ok(())
    }

    /// Take the pending command, if any.
    ///
    /// # Errors
    ///
    /// I/O / JSON.
    pub fn take_control(&self) -> Result<Option<ControlOp>> {
        let path = self.control_path();
        if !path.is_file() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)?;
        fs::remove_file(&path)?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&text)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelforge_capture_core::{CaptureSpec, SessionId, SessionMeta};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn write_then_take() {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rf-ctl-{n}"));
        let store = SessionStore::create(
            &root,
            SessionMeta {
                id: SessionId::new("ses_c"),
                name: "c".into(),
                spec: CaptureSpec::screen(),
                started_unix: None,
                duration: None,
            },
        )
        .unwrap();
        assert!(store.take_control().unwrap().is_none());
        store.write_control(ControlOp::Pause).unwrap();
        store.write_control(ControlOp::Stop).unwrap();
        assert_eq!(store.take_control().unwrap(), Some(ControlOp::Stop));
        assert!(store.take_control().unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
