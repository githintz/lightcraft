//! Network providers against real servers, run only when one is configured (never the internet
//! by default):
//!
//! - `LIGHTCRAFT_TEST_S3=http://127.0.0.1:5000` — an S3-compatible server (MinIO, moto, Garage…);
//!   `LIGHTCRAFT_TEST_S3_KEY` / `_SECRET` default to `test` / `test`, the bucket is
//!   `lightcraft-test`, created if missing.
//! - `LIGHTCRAFT_TEST_WEBDAV=http://127.0.0.1:8081/` — a WebDAV collection the test may write
//!   to (`_USER` / `_PASSWORD` for basic auth, empty by default).
//!
//! - `LIGHTCRAFT_TEST_GDRIVE_REFRESH_TOKEN` with `LIGHTCRAFT_TEST_GDRIVE_CLIENT_ID` (and
//!   `_CLIENT_SECRET` if the client has one) — a real Google account; get a refresh token with
//!   `cargo run -p lightcraft-cloud --example gdrive-signin -- <client id> [<client secret>]`.
//!   Each run makes its own app folder in My Drive (`lightcraft-test-run-…`), which the test
//!   empties but doesn't remove.
//!
//! Each run uses a fresh app folder, so the contract's "starts empty" holds.

#![cfg(not(target_arch = "wasm32"))]

use lightcraft_cloud::gdrive::{GoogleDriveConfig, GoogleDriveProvider, MULTIPART_MAX};
use lightcraft_cloud::oauth::OAuthClient;
use lightcraft_cloud::s3::{S3Config, S3Provider};
use lightcraft_cloud::webdav::{WebDavConfig, WebDavProvider};
use lightcraft_cloud::{Provider, contract, store_blob};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn run_id() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("run-{}-{nanos}", std::process::id())
}

#[test]
fn s3_server_passes_the_contract() {
    let Some(endpoint) = env("LIGHTCRAFT_TEST_S3") else {
        eprintln!("skipped: set LIGHTCRAFT_TEST_S3 to run against an S3-compatible server");
        return;
    };
    let config = S3Config {
        endpoint,
        region: env("LIGHTCRAFT_TEST_S3_REGION").unwrap_or_else(|| "us-east-1".into()),
        bucket: "lightcraft-test".into(),
        prefix: format!("{}/", run_id()),
        access_key: env("LIGHTCRAFT_TEST_S3_KEY").unwrap_or_else(|| "test".into()),
        secret_key: env("LIGHTCRAFT_TEST_S3_SECRET").unwrap_or_else(|| "test".into()),
        path_style: true,
    };
    let mut p = S3Provider::new(config, "live s3").unwrap();
    p.ensure_bucket().unwrap();
    p.ensure_bucket().unwrap(); // a bucket that exists is fine
    contract::check_provider(&mut p).unwrap();

    // more objects than one listing page (1000)
    for i in 0..1005u32 {
        store_blob(&mut p, &i.to_le_bytes()).unwrap();
    }
    let listed = p.list().unwrap();
    assert_eq!(listed.len(), 1005);
    assert_eq!(p.quota().unwrap().used, 1005 * 4);
    for l in listed {
        p.delete(&l.remote).unwrap();
    }
    assert!(p.list().unwrap().is_empty());
}

#[test]
fn webdav_server_passes_the_contract() {
    let Some(url) = env("LIGHTCRAFT_TEST_WEBDAV") else {
        eprintln!("skipped: set LIGHTCRAFT_TEST_WEBDAV to run against a WebDAV server");
        return;
    };
    let base = format!("{}/{}/", url.trim_end_matches('/'), run_id());
    let config = WebDavConfig {
        url: base,
        user: env("LIGHTCRAFT_TEST_WEBDAV_USER").unwrap_or_default(),
        password: env("LIGHTCRAFT_TEST_WEBDAV_PASSWORD").unwrap_or_default(),
    };
    let mut p = WebDavProvider::new(config, "live webdav").unwrap();
    p.ensure_root().unwrap();
    contract::check_provider(&mut p).unwrap();
    for i in 0..50u32 {
        store_blob(&mut p, &i.to_le_bytes()).unwrap();
    }
    assert_eq!(p.list().unwrap().len(), 50);
    let q = p.quota().unwrap();
    assert!(q.used >= 200, "{q:?}");
}

#[test]
fn google_drive_passes_the_contract() {
    let (Some(client_id), Some(refresh)) = (env("LIGHTCRAFT_TEST_GDRIVE_CLIENT_ID"), env("LIGHTCRAFT_TEST_GDRIVE_REFRESH_TOKEN")) else {
        eprintln!("skipped: set LIGHTCRAFT_TEST_GDRIVE_CLIENT_ID and _REFRESH_TOKEN to run against Google Drive");
        return;
    };
    let client = OAuthClient::google_drive(client_id, env("LIGHTCRAFT_TEST_GDRIVE_CLIENT_SECRET"));
    let mut config = GoogleDriveConfig::new(&client, refresh);
    config.folder = format!("lightcraft-test-{}", run_id());
    let mut p = GoogleDriveProvider::new(config, "live drive").unwrap();
    contract::check_provider(&mut p).unwrap();
    // a resumable upload in several chunks
    let big: Vec<u8> = (0..MULTIPART_MAX as u32 + 300_000).map(|i| (i % 253) as u8).collect();
    let mut p = p.with_chunk_size(2 * 1024 * 1024).unwrap();
    let (id, remote) = store_blob(&mut p, &big).unwrap();
    assert_eq!(lightcraft_cloud::fetch_blob(&mut p, &remote, &id, big.len() as u64).unwrap(), big);
    let q = p.quota().unwrap();
    assert!(q.used >= big.len() as u64, "{q:?}");
    p.delete(&remote).unwrap();
    assert!(p.list().unwrap().is_empty());
}
