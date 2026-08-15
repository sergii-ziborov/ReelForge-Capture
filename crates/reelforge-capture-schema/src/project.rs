//! [`CaptureProject`] document (mirror of `reelforge_project::project`).

use crate::ids::{MediaRefId, ProjectId, SequenceId, TimelineTrackId};
use crate::model::{Marker, MediaRef, Metadata, SemanticRef, TimelineItem};
use reelforge_capture_core::{CaptureError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Current `CaptureProject` schema. Must equal `reelforge_project::CAPTURE_PROJECT_VERSION`.
pub const CAPTURE_PROJECT_VERSION: u32 = 1;

/// Kind of timeline track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    /// Picture.
    Video,
    /// Sound.
    Audio,
    /// Captions.
    Subtitle,
}

/// One OTIO-like track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineTrack {
    /// Id.
    pub id: TimelineTrackId,
    /// Video / audio / subtitle.
    pub kind: TrackKind,
    /// Items in record order.
    #[serde(default)]
    pub items: Vec<TimelineItem>,
    /// Soft mute (compile may skip audio).
    #[serde(default)]
    pub muted: bool,
}

impl TimelineTrack {
    /// Empty track.
    #[must_use]
    pub fn new(id: TimelineTrackId, kind: TrackKind) -> Self {
        Self {
            id,
            kind,
            items: Vec::new(),
            muted: false,
        }
    }
}

/// One sequence (a timeline).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sequence {
    /// Id.
    pub id: SequenceId,
    /// Display name.
    pub name: String,
    /// Tracks (bottom → top for video).
    #[serde(default)]
    pub tracks: Vec<TimelineTrack>,
    /// Sequence markers.
    #[serde(default)]
    pub markers: Vec<Marker>,
    /// Optional compose canvas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<(u32, u32)>,
}

impl Sequence {
    /// Empty named sequence.
    #[must_use]
    pub fn new(id: SequenceId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            tracks: Vec::new(),
            markers: Vec::new(),
            canvas: None,
        }
    }
}

/// User-facing project handed to `ReelForge` (`compile_project`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureProject {
    /// Schema version.
    pub version: u32,
    /// Project id.
    pub id: ProjectId,
    /// Display name.
    pub name: String,
    /// Sequences.
    #[serde(default)]
    pub sequences: Vec<Sequence>,
    /// Active sequence (default: first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_sequence: Option<SequenceId>,
    /// Media library.
    #[serde(default)]
    pub media: Vec<MediaRef>,
    /// Project-level markers.
    #[serde(default)]
    pub markers: Vec<Marker>,
    /// Metadata.
    #[serde(default)]
    pub metadata: Metadata,
    /// Semantic references (Intelligence / `SightLoom` ids).
    #[serde(default)]
    pub semantic: Vec<SemanticRef>,
}

impl CaptureProject {
    /// Empty v1 project.
    #[must_use]
    pub fn new(id: ProjectId, name: impl Into<String>) -> Self {
        Self {
            version: CAPTURE_PROJECT_VERSION,
            id,
            name: name.into(),
            sequences: Vec::new(),
            active_sequence: None,
            media: Vec::new(),
            markers: Vec::new(),
            metadata: Metadata::default(),
            semantic: Vec::new(),
        }
    }

    /// Parse JSON (`version: 0` is migrated, as `ReelForge` does).
    ///
    /// # Errors
    ///
    /// JSON or a version this schema cannot represent.
    pub fn from_json(text: &str) -> Result<Self> {
        let mut p: Self = serde_json::from_str(text)?;
        if p.version == 0 {
            p.version = CAPTURE_PROJECT_VERSION;
        }
        if p.version > CAPTURE_PROJECT_VERSION {
            return Err(CaptureError::message(format!(
                "CaptureProject version {} is newer than {CAPTURE_PROJECT_VERSION}",
                p.version
            )));
        }
        Ok(p)
    }

    /// Pretty JSON.
    ///
    /// # Errors
    ///
    /// Serde.
    pub fn to_json_pretty(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Reject documents `ReelForge` could not resolve.
    ///
    /// Checks the version, unique media / clip ids, and that every clip
    /// references a media entry that exists. A dangling `media` id is the
    /// failure mode a hand-built JSON document hits first.
    ///
    /// # Errors
    ///
    /// Version mismatch, duplicate id, or dangling media reference.
    pub fn validate(&self) -> Result<()> {
        if self.version != CAPTURE_PROJECT_VERSION {
            return Err(CaptureError::message(format!(
                "CaptureProject version {} != {CAPTURE_PROJECT_VERSION}",
                self.version
            )));
        }
        let mut known: BTreeSet<&MediaRefId> = BTreeSet::new();
        for m in &self.media {
            if !known.insert(&m.id) {
                return Err(CaptureError::message(format!(
                    "duplicate media id {}",
                    m.id.as_str()
                )));
            }
        }
        let mut clips: BTreeSet<&str> = BTreeSet::new();
        for seq in &self.sequences {
            for track in &seq.tracks {
                for item in &track.items {
                    let TimelineItem::Clip(clip) = item else {
                        continue;
                    };
                    if !known.contains(&clip.media) {
                        return Err(CaptureError::message(format!(
                            "clip {} references unknown media {}",
                            clip.id.as_str(),
                            clip.media.as_str()
                        )));
                    }
                    if !clips.insert(clip.id.as_str()) {
                        return Err(CaptureError::message(format!(
                            "duplicate clip id {}",
                            clip.id.as_str()
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Sequence `ReelForge` would compile.
    ///
    /// # Errors
    ///
    /// No sequences, or an `active_sequence` that does not exist.
    pub fn active(&self) -> Result<&Sequence> {
        if self.sequences.is_empty() {
            return Err(CaptureError::message("project has no sequences"));
        }
        if let Some(id) = &self.active_sequence {
            return self
                .sequences
                .iter()
                .find(|s| &s.id == id)
                .ok_or_else(|| CaptureError::message(format!("unknown sequence {}", id.as_str())));
        }
        Ok(&self.sequences[0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::TimelineClipId;
    use crate::model::{SourceRange, TimelineClip};
    use reelforge_capture_core::{HZ_1K, MediaTime};

    fn clip(id: &str, media: &str) -> TimelineItem {
        TimelineItem::Clip(TimelineClip {
            id: TimelineClipId::new(id),
            media: MediaRefId::new(media),
            source: SourceRange {
                start: MediaTime::zero(HZ_1K),
                duration: MediaTime::from_secs(1.0, HZ_1K).unwrap(),
            },
            retiming: crate::model::Retiming::Identity,
            transition_in: None,
            metadata: Metadata::default(),
        })
    }

    fn with_items(items: Vec<TimelineItem>, media: Vec<MediaRef>) -> CaptureProject {
        let mut p = CaptureProject::new(ProjectId::new("ses_1"), "demo");
        let mut seq = Sequence::new(SequenceId::new("main"), "main");
        let mut track = TimelineTrack::new(TimelineTrackId::new("v0"), TrackKind::Video);
        track.items = items;
        seq.tracks.push(track);
        p.sequences.push(seq);
        p.media = media;
        p
    }

    fn media(id: &str) -> MediaRef {
        MediaRef {
            id: MediaRefId::new(id),
            uri: format!("{id}.mkv"),
            duration: None,
            role: Some("video".into()),
        }
    }

    #[test]
    fn round_trip_keeps_the_document() {
        let p = with_items(vec![clip("c0", "m0")], vec![media("m0")]);
        let back = CaptureProject::from_json(&p.to_json_pretty().unwrap()).unwrap();
        assert_eq!(back, p);
        back.validate().unwrap();
    }

    #[test]
    fn dangling_media_reference_is_rejected() {
        let p = with_items(vec![clip("c0", "missing")], vec![media("m0")]);
        let err = p.validate().unwrap_err().to_string();
        assert!(err.contains("unknown media"), "{err}");
    }

    #[test]
    fn duplicate_clip_id_is_rejected() {
        let p = with_items(vec![clip("c0", "m0"), clip("c0", "m0")], vec![media("m0")]);
        assert!(p.validate().is_err());
    }

    #[test]
    fn future_version_is_refused_and_zero_is_migrated() {
        let mut p = with_items(vec![], vec![]);
        p.version = CAPTURE_PROJECT_VERSION + 1;
        let text = serde_json::to_string(&p).unwrap();
        assert!(CaptureProject::from_json(&text).is_err());

        p.version = 0;
        let text = serde_json::to_string(&p).unwrap();
        assert_eq!(
            CaptureProject::from_json(&text).unwrap().version,
            CAPTURE_PROJECT_VERSION
        );
    }

    #[test]
    fn active_defaults_to_the_first_sequence() {
        let p = with_items(vec![], vec![]);
        assert_eq!(p.active().unwrap().id.as_str(), "main");
    }
}
