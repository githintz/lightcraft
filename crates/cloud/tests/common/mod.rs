//! A tiny HTTP/1.1 server side for the fake storage services the provider tests run against
//! (one request per connection, like `lightcraft-fetch`).

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;

use serde_json::Value;

pub fn decode(s: &str) -> String {
    let b = s.replace('+', " ");
    let b = b.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            out.push(u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap(), 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

pub fn params(query: &str) -> HashMap<String, String> {
    query.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').unwrap_or((p, ""))).map(|(k, v)| (decode(k), decode(v))).collect()
}

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

pub fn read_request(conn: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = conn.read(&mut byte).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&byte[..n]);
    };
    let head = String::from_utf8(buf[..head_end].to_vec()).ok()?;
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let headers: HashMap<String, String> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = conn.read(&mut byte).ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&byte[..n]);
    }
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    Some(Request { method, path: path.to_string(), query: params(query), headers, body })
}

pub fn reply(conn: &mut TcpStream, status: u16, headers: &[(&str, String)], body: &[u8]) {
    let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = conn.write_all(head.as_bytes());
    let _ = conn.write_all(body);
}

pub fn json_reply(conn: &mut TcpStream, status: u16, v: Value) {
    reply(conn, status, &[("Content-Type", "application/json".into())], v.to_string().as_bytes());
}

/// The refresh token the fakes accept.
pub const REFRESH: &str = "good-refresh";

/// A fake token endpoint: grants an access token for [`REFRESH`] (and, when `rotate`, for the
/// rotated refresh tokens it hands out) and records it in `tokens`.
pub fn token_endpoint(conn: &mut TcpStream, req: &Request, client_id: &str, tokens: &mut Vec<String>, rotate: bool) {
    let form = params(std::str::from_utf8(&req.body).unwrap_or_default());
    let refresh = form.get("refresh_token").map(String::as_str).unwrap_or_default();
    let ok = form.get("grant_type").map(String::as_str) == Some("refresh_token")
        && form.get("client_id").map(String::as_str) == Some(client_id)
        && (refresh == REFRESH || (rotate && refresh.starts_with("rotated-")));
    if !ok {
        json_reply(conn, 400, serde_json::json!({"error": "invalid_grant", "error_description": "revoked"}));
        return;
    }
    let token = format!("at-{}", tokens.len() + 1);
    tokens.push(token.clone());
    let mut reply = serde_json::json!({"access_token": token, "expires_in": 3600, "token_type": "Bearer"});
    if rotate {
        reply["refresh_token"] = serde_json::json!(format!("rotated-{}", tokens.len()));
    }
    json_reply(conn, 200, reply);
}

/// The bearer token of a request, if it carries one.
pub fn bearer(req: &Request) -> Option<&str> {
    req.headers.get("authorization").and_then(|a| a.strip_prefix("Bearer "))
}
