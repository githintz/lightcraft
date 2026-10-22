//! Organize photos already in a storage account into albums, without changing them: scan the
//! user's folders ([`scan`]: names, sizes, the provider's content hash, and the metadata at the
//! start of each file, read with ranged requests instead of downloads), then plan albums by trip
//! with duplicates found ([`plan`]). The plan is for the user to review; LightCraft's albums then
//! point at the files where they are.
//!
//! Everything here reads through [`lightcraft_cloud::Browse`], which has no call that changes a
//! folder.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod kind;
pub mod plan;
pub mod report;
pub mod scan;
pub mod video;

pub use kind::Kind;
