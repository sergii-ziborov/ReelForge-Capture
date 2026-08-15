//! `CaptureProject` **v1 wire contract** — the single definition Capture writes
//! and `ReelForge` reads.
//!
//! # Why this crate exists
//!
//! Capture must not path-depend on the `ReelForge` crate graph (the two repos
//! stay independently cloneable), yet both sides must agree on one document
//! shape. Before this crate, Capture hand-built `serde_json::Value` maps that
//! *looked* like `reelforge_project::CaptureProject`; nothing failed when a
//! field name drifted.
//!
//! Now there is one typed definition plus one checked-in golden document
//! (`tests/golden/capture_project_v1.json`). The types mirror
//! `reelforge_project` field for field:
//!
//! | here | `ReelForge` |
//! | --- | --- |
//! | [`CaptureProject`] | `reelforge_project::CaptureProject` |
//! | [`Sequence`] / [`TimelineTrack`] / [`TrackKind`] | same names |
//! | [`TimelineItem`] / [`TimelineClip`] / [`Gap`] / [`NestedSequence`] | same names |
//! | [`MediaRef`] / [`Marker`] / [`SemanticRef`] / [`Metadata`] | same names |
//! | [`SourceRange`] / [`Retiming`] / [`Transition`] | same names |
//! | [`MediaTime`](reelforge_capture_core::MediaTime) | `reelforge_core::MediaTime` (`ticks` + `timescale`) |
//!
//! # Keeping the two repos in sync
//!
//! 1. Change the types here and re-bless the golden document
//!    (`REELFORGE_BLESS=1 cargo test -p reelforge-capture-schema`).
//! 2. Copy the golden document into `ReelForge` and assert
//!    `CaptureProject::from_json` accepts it unchanged.
//! 3. Bump [`CAPTURE_PROJECT_VERSION`] on both sides in the same release.
//!
//! A silent drift now fails a test instead of a customer render.

mod ids;
mod model;
mod project;

pub use ids::{MediaRefId, ProjectId, SequenceId, TimelineClipId, TimelineTrackId};
pub use model::{
    Gap, Marker, MediaRef, Metadata, NestedSequence, Retiming, SemanticRef, SourceRange,
    TimelineClip, TimelineItem, Transition, TransitionKind,
};
pub use project::{CAPTURE_PROJECT_VERSION, CaptureProject, Sequence, TimelineTrack, TrackKind};
