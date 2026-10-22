//! End-to-end test of `lightcraft-cli drive`: the dashboard's HTTP API on a real server process,
//! from setting up the vault to uploading, listing and downloading a file, and the requests it
//! must refuse (another site, another host name, commands outside the drive).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_lightcraft-cli");

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// (status, headers, body) of one request.
fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nContent-Length: {}\r\n", body.len());
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        req.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&out[..split]).to_string();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, head, out[split + 4..].to_vec())
}

fn call(port: u16, command: &str, params: Value) -> Value {
    let body = json!({"command": command, "params": params}).to_string();
    let (status, _, b) = http(port, "POST", "/api/call", &[("X-LightCraft-Drive", "1"), ("Content-Type", "application/json")], body.as_bytes());
    assert_eq!(status, 200, "{command}: {}", String::from_utf8_lossy(&b));
    serde_json::from_slice(&b).unwrap()
}

fn ok(port: u16, command: &str, params: Value) -> Value {
    let r = call(port, command, params);
    assert_eq!(r["ok"], true, "{command}: {r}");
    r["result"].clone()
}

fn settle(port: u16) {
    let start = Instant::now();
    loop {
        let t = ok(port, "drive.transfers", json!({}));
        if t["busy"] == false && t["unsynced"] == false {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "never settled: {t}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_dashboard_api_stores_and_returns_files() {
    let root = std::env::temp_dir().join(format!("lc-drive-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (lib, acc, ui) = (root.join("lib"), root.join("acc"), root.join("ui"));
    std::fs::create_dir_all(ui.join("assets")).unwrap();
    std::fs::write(ui.join("index.html"), "<!doctype html><title>dash</title>").unwrap();
    std::fs::write(ui.join("assets/app.js"), "console.log(1)").unwrap();
    let port = 18_000 + (std::process::id() % 1500) as u16;
    let child = Command::new(BIN)
        .args(["drive", "--library", lib.to_str().unwrap(), "--port", &port.to_string(), "--ui", ui.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _server = Server(child);
    let start = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(start.elapsed() < Duration::from_secs(60), "the server didn't start");
        std::thread::sleep(Duration::from_millis(100));
    }

    // the dashboard's files, and the app for unknown routes
    let (s, head, body) = http(port, "GET", "/assets/app.js", &[], b"");
    assert_eq!((s, body.as_slice()), (200, &b"console.log(1)"[..]));
    assert!(head.contains("text/javascript"));
    assert_eq!(http(port, "GET", "/somewhere", &[], b"").2, b"<!doctype html><title>dash</title>");
    assert_eq!(http(port, "GET", "/../../etc/passwd.txt", &[], b"").0, 404);

    let st = ok(port, "drive.status", json!({}));
    assert_eq!((st["configured"].as_bool(), st["unlocked"].as_bool()), (Some(false), Some(false)));
    ok(port, "cloud.setup", json!({"passphrase": "correct horse battery"}));
    ok(
        port,
        "cloud.addAccount",
        json!({"provider": "folder", "label": "Disk", "config": {"path": acc.join("lc").to_string_lossy()}, "cap": 10_000_000}),
    );
    let folder = ok(port, "drive.mkdir", json!({"name": "Trip notes"}));

    let content = "día 1: playa\n".repeat(500);
    let path = format!("/api/upload?name={}&parent={}", "d%C3%ADa%201.txt", folder["id"].as_str().unwrap());
    let (s, _, b) = http(port, "PUT", &path, &[("X-LightCraft-Drive", "1")], content.as_bytes());
    assert_eq!(s, 200);
    let up: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(up["result"]["name"], "día 1.txt", "{up}");
    settle(port);
    let inbox = lib.join("cloud/inbox");
    assert_eq!(std::fs::read_dir(&inbox).map(|r| r.count()).unwrap_or(0), 0, "the staged upload is removed once stored");

    let listed = ok(port, "drive.list", json!({"folder": folder["id"]}));
    let file = listed["items"][0].clone();
    assert_eq!((file["name"].as_str(), file["size"].as_u64()), (Some("día 1.txt"), Some(content.len() as u64)));
    let task = ok(port, "drive.download", json!({"id": file["id"]}))["task"].as_str().unwrap().to_string();
    settle(port);
    let (s, head, body) = http(port, "GET", &format!("/api/file/{task}"), &[], b"");
    assert_eq!(s, 200);
    assert_eq!(body, content.as_bytes());
    assert!(head.contains("attachment") && head.contains("filename*=UTF-8''d%C3%ADa%201.txt"), "{head}");
    let (s, head, body) = http(port, "GET", &format!("/api/file/{task}?inline=1"), &[("Range", "bytes=0-3")], b"");
    assert_eq!((s, body.as_slice()), (206, "día".as_bytes()));
    assert!(head.contains("inline") && head.contains("sandbox"));

    // refused: no API header, another host name (DNS rebinding), another site, other commands
    let body = json!({"command": "drive.status"}).to_string();
    assert_eq!(http(port, "POST", "/api/call", &[], body.as_bytes()).0, 403);
    assert_eq!(http(port, "POST", "/api/call", &[("X-LightCraft-Drive", "1"), ("Host", "evil.example")], body.as_bytes()).0, 403);
    assert_eq!(http(port, "GET", &format!("/api/file/{task}"), &[("Sec-Fetch-Site", "cross-site")], b"").0, 403);
    for cmd in ["photo.delete", "library.import", "cloud.restore"] {
        let body = json!({"command": cmd}).to_string();
        assert_eq!(http(port, "POST", "/api/call", &[("X-LightCraft-Drive", "1")], body.as_bytes()).0, 403, "{cmd}");
    }
    let _ = std::fs::remove_dir_all(&root);
}
