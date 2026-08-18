//! OTIO-like timeline items (mirror of `reelforge_project::model`).

use crate::ids::{MediaRefId, SequenceId, TimelineClipId};
use reelforge_capture_core::MediaTime;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Free-form metadata (sorted so JSON output is stable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Metadata {
    /// Sorted key/value tags.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
}

impl Metadata {
    /// Metadata from `(key, value)` pairs.
    #[must_use]
    pub fn from_tags<K: Into<String>, V: Into<String>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        Self {
            tags: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// No tags.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
    }
}

/// Intelligence / vision handle (subject, event, query) — not a media primitive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticRef {
    /// Kind (`subject`, `event`, `query`, `policy`).
    pub kind: String,
    /// Opaque id in the owning product.
    pub id: String,
}

impl SemanticRef {
    /// Construct.
    #[must_use]
    pub fn new(kind: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            id: id.into(),
        }
    }
}

/// Source in/out on the media file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRange {
    /// In-point.
    pub start: MediaTime,
    /// Duration from the in-point.
    pub duration: MediaTime,
}

/// Playback rate of a clip on the record timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Retiming {
    /// 1×.
    #[default]
    Identity,
    /// Constant speed (`2.0` = twice as fast).
    Speed {
        /// Factor.
        factor: f64,
    },
    /// Hold the frame at `at` for `hold` (extends record duration).
    Freeze {
        /// Source time of the frozen frame.
        at: MediaTime,
        /// Hold length.
        hold: MediaTime,
    },
    /// Repeat the source (`duration` wins over `times`).
    Loop {
        /// Total output length.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration: Option<MediaTime>,
        /// Repeat count when `duration` is unset.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        times: Option<u32>,
    },
}

/// Media library entry (file / URI).
///
/// One entry addresses exactly one decodable stream. A container that muxes
/// several audio legs is **not** addressable here — Capture demuxes each leg
/// into its own file before writing the project (see `capture audio`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaRef {
    /// Id referenced by clips.
    pub id: MediaRefId,
    /// Host path or URI.
    pub uri: String,
    /// Optional known duration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<MediaTime>,
    /// `video` / `audio` / `proxy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// Incoming transition (fade / dissolve / wipe).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    /// Kind.
    pub kind: TransitionKind,
    /// Overlap duration.
    pub duration: MediaTime,
}

/// Transition flavour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionKind {
    /// Cross-dissolve.
    Dissolve,
    /// Fade through black.
    Fade,
    /// Wipe.
    Wipe,
}

/// Editorial marker (not a render node).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    /// Time on the sequence.
    pub t: MediaTime,
    /// Optional range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<MediaTime>,
    /// Label.
    pub name: String,
    /// Optional semantic handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic: Option<SemanticRef>,
}

/// Empty space on a track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gap {
    /// Duration of silence / empty canvas.
    pub duration: MediaTime,
}

/// Nested sequence reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NestedSequence {
    /// Target sequence.
    pub sequence: SequenceId,
    /// How long it occupies on the parent (`None` = full child duration).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<MediaTime>,
}

/// Pixel crop inside the source frame (`rf.transform.crop`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CropRect {
    /// Left.
    pub x: u32,
    /// Top.
    pub y: u32,
    /// Width.
    pub w: u32,
    /// Height.
    pub h: u32,
}

/// One clip on a track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineClip {
    /// Clip id.
    pub id: TimelineClipId,
    /// Media library entry.
    pub media: MediaRefId,
    /// Source in/out.
    pub source: SourceRange,
    /// Retiming (identity / speed / freeze / loop).
    #[serde(default)]
    pub retiming: Retiming,
    /// Optional incoming transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition_in: Option<Transition>,
    /// Crop applied after trim (click-zoom).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<CropRect>,
    /// Scale the crop back to this size (usually the sequence canvas).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale_to: Option<(u32, u32)>,
    /// Clip metadata.
    #[serde(default)]
    pub metadata: Metadata,
}

/// Item on a timeline track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineItem {
    /// Media clip.
    Clip(TimelineClip),
    /// Gap.
    Gap(Gap),
    /// Nested sequence.
    Nested(NestedSequence),
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelforge_capture_core::HZ_1K;

    #[test]
    fn clip_is_internally_tagged() {
        let item = TimelineItem::Clip(TimelineClip {
            id: TimelineClipId::new("c0"),
            media: MediaRefId::new("seg_000001"),
            source: SourceRange {
                start: MediaTime::zero(HZ_1K),
                duration: MediaTime::from_secs(1.0, HZ_1K).unwrap(),
            },
            retiming: Retiming::Speed { factor: 2.0 },
            transition_in: None,
            crop: None,
            scale_to: None,
            metadata: Metadata::default(),
        });
        let text = serde_json::to_string(&item).unwrap();
        assert!(text.starts_with(r#"{"kind":"clip""#), "{text}");
        assert!(text.contains(r#""mode":"speed""#), "{text}");
        assert!(!text.contains("transition_in"), "{text}");
        let back: TimelineItem = serde_json::from_str(&text).unwrap();
        assert_eq!(back, item);
    }

    #[test]
    fn identity_retiming_is_the_default() {
        assert_eq!(Retiming::default(), Retiming::Identity);
        assert_eq!(
            serde_json::to_string(&Retiming::Identity).unwrap(),
            r#"{"mode":"identity"}"#
        );
    }
}
