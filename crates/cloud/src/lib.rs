//! Opt-in distributed storage for LightCraft: a library's originals, smart previews and catalog
//! spread over the user's own storage accounts (any number, any providers), with no LightCraft
//! server and no LightCraft account. Design: `docs/cloud-storage.md`; on-storage format:
//! `docs/cloud-format.md` (shared with the Storage Distributor web app).
//!
//! This crate is the core, with no UI and no network code of its own yet:
//! - [`Availability`] and [`SpeedClass`]: how quickly a file must come back, how fast an account is;
//! - [`placement`]: which account(s) store a blob (a pure function of the accounts' state);
//! - [`Provider`]: the interface every storage backend implements, blocking (callers run it on a
//!   worker thread, never the UI thread), with [`MemProvider`] (tests, fault injection) and
//!   [`FolderProvider`] (a folder: a second disk, a NAS share, or a folder a sync client uploads);
//! - [`ContentId`] / [`ObjectName`]: blobs are named by the SHA-256 of their content, so a
//!   download is checked against its name ([`fetch_blob`]) and the same bytes are stored once;
//! - [`contract`]: the checks every provider must pass.
//!
//! Everything that comes back from a provider is treated as hostile input: sizes are capped
//! before reading, names are validated, content is verified against its id.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod availability;
mod blob;
pub mod contract;
mod error;
mod folder;
mod memory;
pub mod placement;
mod provider;

pub use availability::{Availability, SpeedClass};
pub use blob::{ContentId, ObjectName, fetch_blob, store_blob};
pub use error::{CloudError, Result};
pub use folder::FolderProvider;
pub use memory::MemProvider;
pub use placement::{PlacementAccount, PlacementError, PlacementRequest, Weights, choose, usable_free};
pub use provider::{Listed, Provider, ProviderInfo, Quota, RemoteRef};
