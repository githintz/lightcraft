//! Network providers against real servers, run only when one is configured (never the internet
//! by default):
//!
//! - `LIGHTCRAFT_TEST_S3=http://127.0.0.1:5000` — an S3-compatible server (MinIO, moto, Garage…);
//!   `LIGHTCRAFT_TEST_S3_KEY` / `_SECRET` default to `test` / `test`, the bucket is
//!   `lightcraft-test`, created if missing.
//! - `LIGHTCRAFT_TEST_WEBDAV=http://127.0.0.1:8081/` — a WebDAV collection the test may write
//!   to (`_USER` / `_PASSWORD` for basic auth, empty by default).
//!
//! Each run uses a fresh app folder, so the contract's "starts empty" holds.

#![cfg(not(target_arch = "wasm32"))]

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
