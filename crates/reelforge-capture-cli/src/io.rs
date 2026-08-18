//! Edit-list files and video-source CLI parsing.

use reelforge_capture_core::{CaptureError, CaptureSpec, Region, Result, VideoSource};
use reelforge_capture_edit::EditList;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn parse_video(
    screen: bool,
    window: Option<String>,
    region: Option<String>,
) -> Result<VideoSource> {
    if let Some(r) = region {
        let parts: Vec<_> = r.split(',').collect();
        if parts.len() != 4 {
            return Err(CaptureError::message("region must be x,y,w,h"));
        }
        let x: i32 = parts[0]
            .parse()
            .map_err(|_| CaptureError::message("region x"))?;
        let y: i32 = parts[1]
            .parse()
            .map_err(|_| CaptureError::message("region y"))?;
        let w: u32 = parts[2]
            .parse()
            .map_err(|_| CaptureError::message("region w"))?;
        let h: u32 = parts[3]
            .parse()
            .map_err(|_| CaptureError::message("region h"))?;
        return Ok(VideoSource::Region {
            region: Region::new(x, y, w, h),
        });
    }
    if let Some(title) = window {
        return Ok(VideoSource::Window { title });
    }
    let _ = screen;
    Ok(VideoSource::Screen)
}

pub(crate) fn spec_name(spec: &CaptureSpec) -> String {
    match &spec.video {
        VideoSource::Screen => "screen".into(),
        VideoSource::Window { title } => format!("window:{title}"),
        VideoSource::Region { .. } => "region".into(),
    }
}

pub(crate) fn load_edits(session: &Path) -> Result<EditList> {
    let p = session.join("edits.json");
    if !p.is_file() {
        return Ok(EditList::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(p)?)?)
}

pub(crate) fn save_edits(session: &Path, list: &EditList) -> Result<()> {
    fs::write(
        session.join("edits.json"),
        serde_json::to_string_pretty(list)?,
    )?;
    Ok(())
}

pub(crate) fn click_zoom_path(session: &Path) -> PathBuf {
    session.join("click_zoom.json")
}

/// Resolve `--session ID` or a session directory.
///
/// An existing directory with a manifest / WAL wins. Otherwise the id is
/// looked up under `--dir` (default `sessions/`). Host should not invent
/// this path by globbing `sessions/<id>/segments/`.
pub(crate) fn resolve_session(session: &str, dir: &Path) -> Result<PathBuf> {
    let given = Path::new(session);
    if is_session_dir(given) {
        return Ok(given.to_path_buf());
    }
    let under = dir.join(session);
    if is_session_dir(&under) {
        return Ok(under);
    }
    Err(CaptureError::message(format!(
        "session not found: {session} (tried {} and {})",
        given.display(),
        under.display()
    )))
}

fn is_session_dir(path: &Path) -> bool {
    path.is_dir() && (path.join("manifest.json").is_file() || path.join("wal.jsonl").is_file())
}
