//! `lightcraft-cli drive` against hostile and unusual HTTP: malformed requests, uploads cut off
//! or badly named, ranges and download headers, static files, commands that would block the
//! session thread or read arbitrary local files, and more connections than it serves at once.
//! Scenarios C2–C9 of `docs/cloud-test-plan.md`.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_lightcraft-cli");
const API: (&str, &str) = ("X-LightCraft-Drive", "1");

struct Server {
    child: Child,
    port: u16,
    root: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A server on a new library with a dashboard folder; `ready` sets up the vault and a folder account.
fn server(tag: &str, offset: u16, ready: bool) -> Server {
    let root = std::env::temp_dir().join(format!("lc-drive-http-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (lib, ui) = (root.join("lib"), root.join("ui"));
    std::fs::create_dir_all(ui.join("assets")).unwrap();
    std::fs::write(ui.join("index.html"), "<!doctype html><title>dash</title>").unwrap();
    std::fs::write(ui.join("assets/app.js"), "console.log(1)").unwrap();
    std::fs::write(ui.join(".secret"), "no").unwrap();
    std::fs::write(root.join("outside.txt"), "outside the dashboard").unwrap();
    let port = 19_000 + (std::process::id() % 100) as u16 * 8 + offset;
    let child = Command::new(BIN)
        .args(["drive", "--library", lib.to_str().unwrap(), "--port", &port.to_string(), "--ui", ui.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let s = Server { child, port, root };
    let start = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(start.elapsed() < Duration::from_secs(60), "the server didn't start");
        std::thread::sleep(Duration::from_millis(100));
    }
    if ready {
        ok(port, "cloud.setup", json!({"passphrase": "correct horse battery"}));
        let acc = s.root.join("acc/lc");
        ok(port, "cloud.addAccount", json!({"provider": "folder", "label": "Disk", "config": {"path": acc.to_string_lossy()}, "cap": 50_000_000}));
    }
    s
}

/// (status, head, body) for raw request bytes; the write side is closed after them.
fn raw(port: u16, bytes: &[u8]) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let _ = s.write_all(bytes);
    let _ = s.shutdown(Shutdown::Write);
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    parse(&out)
}

fn parse(out: &[u8]) -> (u16, String, Vec<u8>) {
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or_else(|| panic!("no answer: {:?}", String::from_utf8_lossy(out)));
    let head = String::from_utf8_lossy(&out[..split]).to_string();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, head, out[split + 4..].to_vec())
}

fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, String, Vec<u8>) {
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    let mut bytes = req.into_bytes();
    bytes.extend_from_slice(body);
    raw(port, &bytes)
}

fn call(port: u16, command: &str, params: Value) -> (u16, Value) {
    let body = json!({"command": command, "params": params}).to_string();
    let (status, _, b) = http(port, "POST", "/api/call", &[API], body.as_bytes());
    (status, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

fn ok(port: u16, command: &str, params: Value) -> Value {
    let (status, r) = call(port, command, params);
    assert_eq!((status, &r["ok"]), (200, &json!(true)), "{command}: {r}");
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

fn upload(port: u16, query: &str, body: &[u8]) -> (u16, Value) {
    let (status, _, b) = http(port, "PUT", &format!("/api/upload?{query}"), &[API], body);
    (status, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

fn inbox_files(s: &Server) -> usize {
    std::fs::read_dir(s.root.join("lib/cloud/inbox")).map(|r| r.count()).unwrap_or(0)
}

fn names(port: u16) -> Vec<String> {
    let mut n: Vec<String> = ok(port, "drive.list", json!({"folder": "root"}))["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_string())
        .collect();
    n.sort();
    n
}

#[test]
fn malformed_requests() {
    let s = server("malformed", 0, false);
    let p = s.port;
    let host = format!("Host: 127.0.0.1:{p}\r\n");
    // nothing at all, a request line that isn't one
    assert_eq!(raw(p, b"").0, 400);
    assert_eq!(raw(p, b"HELLO\r\n\r\n").0, 400);
    // a head over 16 KiB, more than 100 headers
    let big = format!("GET / HTTP/1.1\r\n{host}X-Big: {}\r\n\r\n", "a".repeat(20_000));
    assert_eq!(raw(p, big.as_bytes()).0, 431);
    let many: String = (0..120).map(|i| format!("X-{i}: v\r\n")).collect();
    assert_eq!(raw(p, format!("GET / HTTP/1.1\r\n{host}{many}\r\n").as_bytes()).0, 431);
    // broken escapes in the path or the query
    assert_eq!(http(p, "GET", "/api/%zz", &[API], b"").0, 400);
    assert_eq!(http(p, "GET", "/x?a=%E2%28", &[], b"").0, 400);
    // no Content-Length, too large, not JSON, no command, params that aren't an object
    let r = raw(p, format!("POST /api/call HTTP/1.1\r\n{host}X-LightCraft-Drive: 1\r\n\r\n{{}}").as_bytes());
    assert_eq!(r.0, 411);
    let r = raw(p, format!("POST /api/call HTTP/1.1\r\n{host}X-LightCraft-Drive: 1\r\nContent-Length: 2000000\r\n\r\n").as_bytes());
    assert_eq!(r.0, 413);
    assert_eq!(http(p, "POST", "/api/call", &[API], b"not json").0, 400);
    assert_eq!(http(p, "POST", "/api/call", &[API], b"{\"params\":{}}").0, 400);
    assert_eq!(http(p, "POST", "/api/call", &[API], b"[1,2]").0, 400);
    let (st, _, b) = http(p, "POST", "/api/call", &[API], br#"{"command":"drive.status","params":[1]}"#);
    assert_eq!(st, 200, "{}", String::from_utf8_lossy(&b));
    // a body shorter than its Content-Length: an error, not a hang
    let r = raw(p, format!("POST /api/call HTTP/1.1\r\n{host}X-LightCraft-Drive: 1\r\nContent-Length: 50\r\n\r\n{{\"command\"").as_bytes());
    assert_eq!(r.0, 400);
    // other methods, other API routes, no Host
    assert_eq!(http(p, "DELETE", "/index.html", &[], b"").0, 405);
    assert_eq!(http(p, "GET", "/api/nothing", &[API], b"").0, 404);
    assert_eq!(raw(p, b"GET / HTTP/1.0\r\n\r\n").0, 403);
    // and it still serves
    assert_eq!(ok(p, "drive.status", json!({}))["configured"], false);
}

#[test]
fn uploads() {
    let s = server("uploads", 1, true);
    let p = s.port;
    // cut off mid-body: refused, nothing left in the inbox
    let head = format!("PUT /api/upload?name=cut.bin HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nX-LightCraft-Drive: 1\r\nContent-Length: 100000\r\n\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(&[7u8; 1000]);
    let (st, _, b) = raw(p, &bytes);
    assert_eq!(st, 400, "{}", String::from_utf8_lossy(&b));
    assert!(String::from_utf8_lossy(&b).contains("cut off"));
    assert_eq!(inbox_files(&s), 0);
    // no name, an empty name, one that is too large
    assert_eq!(upload(p, "", b"x").0, 400);
    assert_eq!(upload(p, "name=%20%20", b"x").0, 400);
    let r = raw(
        p,
        format!("PUT /api/upload?name=huge HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nX-LightCraft-Drive: 1\r\nContent-Length: 5000000000\r\n\r\n")
            .as_bytes(),
    );
    assert_eq!(r.0, 413);
    // hostile names stay names: no folders, no climbing out
    for (name, want) in [("..%2Fx", "..-x"), ("a%2Fb", "a-b"), ("a%5Cc", "a-c"), ("%2E%2E", "")] {
        let (st, r) = upload(p, &format!("name={name}"), b"hostile");
        assert_eq!(st, 200);
        if want.is_empty() {
            assert_eq!(r["ok"], false, "{name}: {r}");
        } else {
            assert_eq!(r["result"]["name"], want, "{name}: {r}");
        }
    }
    // an empty file, a missing folder, copies that aren't a number
    let (_, r) = upload(p, "name=empty.txt&copies=lots", b"");
    assert_eq!(r["ok"], true, "{r}");
    let (_, r) = upload(p, "name=lost.txt&parent=drive.0000000000000000", b"lost");
    assert_eq!(r["ok"], false, "{r}");
    settle(p);
    assert_eq!(inbox_files(&s), 0, "refused and stored uploads leave nothing in the inbox");
    // two at once with the same name: both kept
    let t: Vec<_> = (0..2).map(|i| std::thread::spawn(move || upload(p, "name=same.txt", format!("copy {i}").as_bytes()))).collect();
    for h in t {
        assert_eq!(h.join().unwrap().1["ok"], true);
    }
    settle(p);
    let n = names(p);
    assert!(n.contains(&"same.txt".to_string()) && n.contains(&"same (1).txt".to_string()), "{n:?}");
    assert!(n.contains(&"empty.txt".to_string()) && !n.contains(&"lost.txt".to_string()), "{n:?}");
    // a file on this computer can't be handed over by path, staged or not, nor waited on
    let outside = s.root.join("outside.txt");
    for params in [json!({"path": outside.to_string_lossy()}), json!({"path": outside.to_string_lossy(), "staged": true})] {
        assert_eq!(call(p, "drive.upload", params).0, 403);
    }
    assert!(outside.is_file(), "a staged upload deletes its file: never one outside the inbox");
    assert_eq!(call(p, "drive.wait", json!({})).0, 403);
    // locked: refused, nothing staged
    ok(p, "cloud.lock", json!({}));
    let (_, r) = upload(p, "name=locked.txt", b"locked");
    assert_eq!(r["ok"], false, "{r}");
    assert_eq!(inbox_files(&s), 0);
}

#[test]
fn downloads() {
    let s = server("downloads", 2, true);
    let p = s.port;
    let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
    let mut tasks = std::collections::HashMap::new();
    for (name, body) in [("data.bin", data.as_slice()), ("page.html", b"<script>alert(1)</script>".as_slice()), ("doc.pdf", b"%PDF-1.4\n".as_slice())]
    {
        let (_, r) = upload(p, &format!("name={name}"), body);
        assert_eq!(r["ok"], true, "{r}");
    }
    let (_, r) = upload(p, "name=he%20said%20%22hi%22%20%E2%9C%93%0D%0AX-Evil%3A%201.txt", b"quoted");
    assert_eq!(r["ok"], true, "{r}");
    settle(p);
    for item in ok(p, "drive.list", json!({"folder": "root"}))["items"].as_array().unwrap() {
        let t = ok(p, "drive.download", json!({"id": item["id"]}))["task"].as_str().unwrap().to_string();
        tasks.insert(item["name"].as_str().unwrap().to_string(), t);
    }
    settle(p);
    let url = |name: &str| format!("/api/file/{}", tasks[name]);
    let get = |name: &str, range: Option<&str>| {
        let h: Vec<(&str, &str)> = range.map(|r| vec![("Range", r)]).unwrap_or_default();
        http(p, "GET", &url(name), &h, b"")
    };
    // whole, start, middle, suffix, open-ended, past the end, backwards, malformed
    assert_eq!(get("data.bin", None).2, data);
    let (st, head, body) = get("data.bin", Some("bytes=0-9"));
    assert_eq!((st, body.as_slice()), (206, &data[..10]));
    assert!(head.contains("Content-Range: bytes 0-9/1000"), "{head}");
    assert_eq!(get("data.bin", Some("bytes=500-509")).2, &data[500..510]);
    assert_eq!(get("data.bin", Some("bytes=-5")).2, &data[995..]);
    assert_eq!(get("data.bin", Some("bytes=990-")).2, &data[990..]);
    assert_eq!(get("data.bin", Some("bytes=990-5000")).2, &data[990..]);
    for bad in ["bytes=1000-", "bytes=9-1", "bytes=a-b", "bytes=-"] {
        let (st, head, _) = get("data.bin", Some(bad));
        assert_eq!(st, 416, "{bad}");
        assert!(head.contains("bytes */1000"), "{bad}: {head}");
    }
    // several ranges or another unit: the whole file
    for whole in ["bytes=0-1,5-6", "items=0-1"] {
        let (st, _, body) = get("data.bin", Some(whole));
        assert_eq!((st, body.len()), (200, 1000), "{whole}");
    }
    // HEAD: the headers only
    let (st, head, body) = http(p, "HEAD", &url("data.bin"), &[], b"");
    assert_eq!((st, body.len()), (200, 0));
    assert!(head.contains("Content-Length: 1000") && head.contains("Accept-Ranges: bytes"), "{head}");
    // quotes, Unicode and a line break in the name: one header, both forms of the name
    let quoted = tasks.keys().find(|k| k.starts_with("he said")).unwrap().clone();
    let (st, head, _) = get(&quoted, None);
    assert_eq!(st, 200);
    assert!(!head.contains("\r\nX-Evil"), "the name can't add a header: {head}");
    let disposition = head.lines().find(|l| l.starts_with("Content-Disposition")).unwrap();
    assert!(
        disposition.contains("filename=\"he said _hi_ _") && disposition.contains("filename*=UTF-8''he%20said%20%22hi%22%20%E2%9C%93"),
        "{disposition}"
    );
    // shown inline: HTML as sandboxed text, a PDF as a PDF; never inline without asking
    let (_, head, _) = http(p, "GET", &format!("{}?inline=1", url("page.html")), &[], b"");
    assert!(head.contains("text/plain") && head.contains("sandbox") && head.contains("inline"), "{head}");
    let (_, head, _) = http(p, "GET", &format!("{}?inline=1", url("doc.pdf")), &[], b"");
    assert!(head.contains("application/pdf") && !head.contains("sandbox"), "{head}");
    let (_, head, _) = get("page.html", None);
    assert!(head.contains("application/octet-stream") && head.contains("attachment"), "{head}");
    // unknown or malformed tasks
    assert_eq!(http(p, "GET", "/api/file/t-nope", &[], b"").0, 404);
    assert_eq!(http(p, "GET", "/api/file/..%2F..%2Fetc%2Fpasswd", &[], b"").0, 404);
}

#[test]
fn static_files() {
    let s = server("static", 3, false);
    let p = s.port;
    let index = b"<!doctype html><title>dash</title>".to_vec();
    let (st, head, body) = http(p, "GET", "/", &[], b"");
    assert_eq!((st, &body), (200, &index));
    assert!(head.contains("Content-Security-Policy") && head.contains("frame-ancestors 'none'"), "{head}");
    assert_eq!(http(p, "GET", "/drive/abc", &[], b"").2, index, "app routes are the app");
    assert_eq!(http(p, "GET", "/assets", &[], b"").2, index);
    for missing in ["/assets/missing.js", "/.secret", "/%2E%2E/outside.txt", "/assets/..%2F..%2Foutside.txt", "/x%5C..%5Coutside.txt", "/C:/x.txt"] {
        assert_eq!(http(p, "GET", missing, &[], b"").0, 404, "{missing}");
    }
    let (st, head, body) = http(p, "HEAD", "/assets/app.js", &[], b"");
    assert_eq!((st, body.len()), (200, 0));
    assert!(head.contains("immutable") && head.contains("Content-Length: 14"), "{head}");
}

#[test]
fn waits_dont_block_the_server() {
    let s = server("waits", 4, true);
    let p = s.port;
    // `wait` is dropped: these answer at once, and other requests keep being served
    let start = Instant::now();
    ok(p, "cloud.refresh", json!({"wait": true}));
    let start_backup = Instant::now();
    loop {
        let r = call(p, "cloud.backup", json!({"wait": true})).1;
        if r["ok"] == true {
            assert!(r["result"].get("running").is_some(), "{r}");
            break;
        }
        // the account check may still be running
        assert!(start_backup.elapsed() < Duration::from_secs(20), "{r}");
        std::thread::sleep(Duration::from_millis(50));
    }
    ok(p, "drive.status", json!({}));
    assert!(start.elapsed() < Duration::from_secs(20), "{:?}", start.elapsed());
    let start = Instant::now();
    loop {
        if ok(p, "cloud.progress", json!({}))["running"] == false {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(60), "the backup never ended");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn many_connections() {
    let s = server("many", 5, false);
    let p = s.port;
    // 64 clients that connect and send nothing hold every slot
    let idle: Vec<TcpStream> = (0..64).map(|_| TcpStream::connect(("127.0.0.1", p)).unwrap()).collect();
    std::thread::sleep(Duration::from_millis(300));
    let (st, _, _) = http(p, "GET", "/", &[], b"");
    assert_eq!(st, 503);
    // once they go, it serves again
    drop(idle);
    let start = Instant::now();
    loop {
        if http(p, "GET", "/", &[], b"").0 == 200 {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "it never recovered");
        std::thread::sleep(Duration::from_millis(50));
    }
}
