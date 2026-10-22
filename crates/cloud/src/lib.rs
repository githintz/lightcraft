//! Opt-in distributed storage for LightCraft: a library's originals, smart previews and catalog
//! spread over the user's own storage accounts (any number, any providers), with no LightCraft
//! server and no LightCraft account. Design: `docs/cloud-storage.md`; on-storage format:
//! `docs/cloud-format.md` (shared with the Storage Distributor web app).
//!
//! This crate is the core, with no UI:
//! - [`Availability`] and [`SpeedClass`]: how quickly a file must come back, how fast an account is;
//! - [`placement`]: which account(s) store a blob (a pure function of the accounts' state);
//! - [`Provider`]: the interface every storage backend implements, blocking (callers run it on a
//!   worker thread, never the UI thread), with [`MemProvider`] (tests, fault injection) and
//!   [`FolderProvider`] (a folder: a second disk, a NAS share, or a folder a sync client uploads);
//! - [`ContentId`] / [`ObjectName`]: blobs are named by the SHA-256 of their content, so a
//!   download is checked against its name ([`fetch_blob`]) and the same bytes are stored once;
//! - [`contract`]: the checks every provider must pass;
//! - [`backup`]: backup and restore over a [`backup::Fleet`] of accounts (placement, encrypted
//!   blobs, the manifest on every account); [`accounts`] opens a vault's accounts (native);
//! - native only: the network providers [`s3`], [`webdav`], [`gdrive`] (Google Drive),
//!   [`onedrive`] and [`dropbox`], and
//!   [`oauth`] (sign-in for consumer drives, with no LightCraft server).
//!
//! Everything that comes back from a provider is treated as hostile input: sizes are capped
//! before reading, names are validated, content is verified against its id.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(not(target_arch = "wasm32"))]
pub mod accounts;
mod availability;
pub mod backup;
mod blob;
pub mod browse;
pub mod contract;
pub mod crypto;
#[cfg(not(target_arch = "wasm32"))]
pub mod dropbox;
mod error;
mod folder;
#[cfg(not(target_arch = "wasm32"))]
pub mod gdrive;
pub mod manifest;
mod memory;
#[cfg(not(target_arch = "wasm32"))]
mod net;
#[cfg(not(target_arch = "wasm32"))]
pub mod oauth;
#[cfg(not(target_arch = "wasm32"))]
pub mod onedrive;
#[cfg(not(target_arch = "wasm32"))]
pub mod opendrive;
pub mod placement;
mod provider;
#[cfg(not(target_arch = "wasm32"))]
pub mod s3;
pub mod vault;
#[cfg(not(target_arch = "wasm32"))]
pub mod webdav;

pub use availability::{Availability, SpeedClass};
pub use blob::{ContentId, ObjectName, fetch_blob, store_blob};
pub use browse::{Browse, BrowseDir, BrowseFile, FolderBrowser};
pub use error::{CloudError, Result};
pub use folder::FolderProvider;
pub use manifest::{Entry, EntryKind, Manifest, Op, OpKind, Stamp};
pub use memory::MemProvider;
#[cfg(not(target_arch = "wasm32"))]
pub use net::Net;
pub use placement::{PlacementAccount, PlacementError, PlacementRequest, Weights, choose, usable_free};
pub use provider::{Listed, Provider, ProviderInfo, Quota, RemoteRef};
pub use vault::{AccountSecret, Vault, VaultParams};
