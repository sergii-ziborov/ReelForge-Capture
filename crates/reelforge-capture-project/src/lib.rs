//! Headless `ReelForge` `CaptureProject` document (JSON, no GUI).

use reelforge_capture_core::{Result, SessionId};
use reelforge_capture_edit::{ClickZoom, KeptRange};
use reelforge_capture_store::SessionStore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// `ReelForge`-compatible project file (schema v1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeadlessProject {
    /// `CAPTURE_PROJECT_VERSION`.
    pub version: u32,
    /// Project id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Sequences.
    pub sequences: Vec<Value>,
    /// Media library.
    pub media: Vec<Value>,
    /// Semantic refs (Intelligence handles; empty unless the host filled them).
    #[serde(default)]
    pub semantic: Vec<Value>,
}

/// Build a single-sequence project from kept ranges + optional click zooms.
///
/// Each kept range is a clip. Speed becomes `retiming.speed`. Click zooms are
/// markers (`ReelForge` compile stays deterministic; the host / Intelligence
/// can promote them to crop ops).
///
/// # Errors
///
/// JSON / missing session id.
pub fn project_from_session(
    store: &SessionStore,
    kept: &[KeptRange],
    zooms: &[ClickZoom],
) -> Result<HeadlessProject> {
    let meta = &store.manifest().meta;
    let media_id = "cap_src";
    let uri = first_segment_uri(store, meta.id.as_str());
    let media = vec![json!({
        "id": media_id,
        "uri": uri,
        "role": "video",
    })];

    let mut items = Vec::new();
    for (i, k) in kept.iter().enumerate() {
        let retiming = if (k.speed - 1.0).abs() < 1e-9 {
            json!({ "mode": "identity" })
        } else {
            json!({ "mode": "speed", "factor": k.speed })
        };
        items.push(json!({
            "kind": "clip",
            "id": format!("c{i}"),
            "media": media_id,
            "source": {
                "start": { "ticks": k.source.start.ticks, "timescale": k.source.start.timescale },
                "duration": {
                    "ticks": k.source.end.ticks - k.source.start.ticks,
                    "timescale": k.source.start.timescale
                }
            },
            "retiming": retiming
        }));
    }

    let markers: Vec<Value> = zooms
        .iter()
        .enumerate()
        .map(|(i, z)| {
            json!({
                "t": { "ticks": z.range.start.ticks, "timescale": z.range.start.timescale },
                "duration": {
                    "ticks": z.range.end.ticks - z.range.start.ticks,
                    "timescale": z.range.start.timescale
                },
                "name": format!("click_zoom_{i}"),
                "semantic": { "kind": "event", "id": format!("click:{0},{1}", z.x, z.y) }
            })
        })
        .collect();

    let seq = json!({
        "id": "main",
        "name": "main",
        "tracks": [{
            "id": "v0",
            "kind": "video",
            "items": items,
            "muted": false
        }],
        "markers": markers
    });

    Ok(HeadlessProject {
        version: 1,
        id: meta.id.as_str().to_string(),
        name: meta.name.clone(),
        sequences: vec![seq],
        media,
        semantic: Vec::new(),
    })
}

/// Pretty JSON for `CaptureProject::from_json`.
///
/// # Errors
///
/// Serde.
pub fn to_json_pretty(project: &HeadlessProject) -> Result<String> {
    Ok(serde_json::to_string_pretty(project)?)
}

fn first_segment_uri(store: &SessionStore, id: &str) -> String {
    store.manifest().segments.first().map_or_else(
        || format!("sessions/{id}/segments/000001.mkv"),
        |s| {
            store
                .root()
                .join(&s.path)
                .to_string_lossy()
                .replace('\\', "/")
        },
    )
}

/// Convenience: empty project id wrapper.
#[must_use]
pub fn session_project_id(id: &SessionId) -> String {
    id.as_str().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelforge_capture_core::{
        CaptureSpec, HZ_1K, MediaRange, MediaTime, SegmentId, SessionMeta,
    };
    use reelforge_capture_edit::KeptRange;
    use reelforge_capture_store::{SegmentRecord, SessionStore};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn emits_clip_and_speed() {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rf-prj-{n}"));
        let mut store = SessionStore::create(
            &root,
            SessionMeta {
                id: SessionId::new("ses_p"),
                name: "demo".into(),
                spec: CaptureSpec::screen(),
                started_unix: None,
                duration: None,
            },
        )
        .unwrap();
        store
            .commit_segment(SegmentRecord {
                id: SegmentId::first(),
                path: "segments/000001.mkv".into(),
                start: MediaTime::zero(HZ_1K),
                end: MediaTime::from_secs(10.0, HZ_1K).unwrap(),
            })
            .unwrap();
        let kept = [KeptRange {
            source: MediaRange::new(
                MediaTime::from_secs(1.0, HZ_1K).unwrap(),
                MediaTime::from_secs(4.0, HZ_1K).unwrap(),
            )
            .unwrap(),
            speed: 2.0,
        }];
        let p = project_from_session(&store, &kept, &[]).unwrap();
        let text = to_json_pretty(&p).unwrap();
        assert!(text.contains("\"mode\": \"speed\""));
        assert!(text.contains("\"factor\": 2.0"));
        assert_eq!(p.version, 1);
        let _ = std::fs::remove_dir_all(root);
    }
}
