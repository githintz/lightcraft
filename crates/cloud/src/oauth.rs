//! OAuth 2.0 for an installed app: the authorization code flow with PKCE (RFC 7636) and a
//! loopback redirect (RFC 8252 §7.3), so no LightCraft server is ever in the path. Native only.
//!
//! 1. [`LoopbackFlow::start`] listens on `127.0.0.1` at a port the system picks and builds the
//!    sign-in URL; the caller opens it in the user's browser (this crate never opens one).
//! 2. [`LoopbackFlow::wait`] (on a worker thread) takes the browser's redirect, checks its
//!    `state`, answers with a short page, and trades the code for tokens.
//! 3. The refresh token goes into the vault; [`refresh`] gets a fresh access token from it.
//!
//! The provider's client id is public (as RFC 8252 expects of installed apps); a client secret,
//! when the provider issues one for desktop apps (Google does), is not a secret either. Users may
//! bring their own client id.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use lightcraft_fetch::http::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::crypto::Random;
use crate::net::{MAX_META_BODY, Net, base64url, form, parse_form};
use crate::{CloudError, Result};

/// Largest request line + headers read from the browser's redirect.
const MAX_REDIRECT_REQUEST: usize = 16 * 1024;
/// Most connections to the loopback port before giving up (favicon requests, stray clients).
const MAX_REDIRECT_CONNECTIONS: usize = 64;

/// A provider's OAuth endpoints and this app's registration with it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthClient {
    pub auth_url: String,
    pub token_url: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
    /// Extra sign-in URL parameters (Google: `access_type=offline`, `prompt=consent`, so a
    /// refresh token always comes back).
    #[serde(default)]
    pub extra_auth_params: Vec<(String, String)>,
    /// A fixed loopback port, for providers that match redirect URIs exactly; `None` lets the
    /// system pick a free one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_port: Option<u16>,
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient")
            .field("auth_url", &self.auth_url)
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

impl OAuthClient {
    /// Google, for Drive access limited to the files LightCraft creates
    /// ([`crate::gdrive::SCOPE`]).
    pub fn google_drive(client_id: impl Into<String>, client_secret: Option<String>) -> OAuthClient {
        OAuthClient {
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            client_id: client_id.into(),
            client_secret,
            scopes: vec![crate::gdrive::SCOPE.into()],
            // consent: a refresh token every time; select_account: a second account can be added
            extra_auth_params: vec![("access_type".into(), "offline".into()), ("prompt".into(), "consent select_account".into())],
            redirect_port: None,
        }
    }

    /// Microsoft (OneDrive personal and work/school), for the app's own folder
    /// ([`crate::onedrive::SCOPES`]). A public client: no secret.
    pub fn onedrive(client_id: impl Into<String>) -> OAuthClient {
        OAuthClient {
            auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize".into(),
            token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token".into(),
            client_id: client_id.into(),
            client_secret: None,
            scopes: crate::onedrive::SCOPES.iter().map(|s| s.to_string()).collect(),
            extra_auth_params: vec![("prompt".into(), "select_account".into())],
            redirect_port: None,
        }
    }

    /// OpenDrive's own accounts (partner id [`crate::opendrive::CLIENT_ID`]), whose only grant is
    /// [`password_grant`]; `token_url` is under the API root.
    pub fn opendrive(token_url: impl Into<String>) -> OAuthClient {
        OAuthClient {
            auth_url: String::new(),
            token_url: token_url.into(),
            client_id: crate::opendrive::CLIENT_ID.into(),
            client_secret: None,
            scopes: Vec::new(),
            extra_auth_params: Vec::new(),
            redirect_port: None,
        }
    }

    /// Dropbox, for an app registered with "App folder" access ([`crate::dropbox::SCOPES`]).
    /// Dropbox matches redirect URIs exactly, so the app's registration must list
    /// `http://127.0.0.1:<redirect_port>`.
    pub fn dropbox(client_id: impl Into<String>, redirect_port: u16) -> OAuthClient {
        OAuthClient {
            auth_url: "https://www.dropbox.com/oauth2/authorize".into(),
            token_url: "https://api.dropboxapi.com/oauth2/token".into(),
            client_id: client_id.into(),
            client_secret: None,
            scopes: crate::dropbox::SCOPES.iter().map(|s| s.to_string()).collect(),
            // offline: a refresh token; force_reauthentication: a second account can be added
            extra_auth_params: vec![("token_access_type".into(), "offline".into()), ("force_reauthentication".into(), "true".into())],
            redirect_port: Some(redirect_port),
        }
    }
}

/// What a token endpoint returns. `Debug` never prints the tokens.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    /// Absent on a refresh that keeps the old refresh token.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Seconds the access token is valid for, when the provider says.
    #[serde(default)]
    pub expires_in: Option<u64>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens").field("refresh_token", &self.refresh_token.is_some()).field("expires_in", &self.expires_in).finish_non_exhaustive()
    }
}

/// An access token kept fresh from a refresh token, for the providers that sign in with OAuth.
/// A refresh token the provider rotates is kept ([`TokenSource::refresh_token`] then returns the
/// new one, for the vault).
pub struct TokenSource {
    client: OAuthClient,
    refresh_token: String,
    access: Option<(String, Instant)>,
}

impl std::fmt::Debug for TokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSource").field("client", &self.client).finish_non_exhaustive()
    }
}

impl TokenSource {
    pub fn new(client: OAuthClient, refresh_token: impl Into<String>) -> TokenSource {
        TokenSource { client, refresh_token: refresh_token.into(), access: None }
    }

    pub fn client(&self) -> &OAuthClient {
        &self.client
    }

    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    /// A valid access token, fetched when there is none or it is about to expire.
    pub fn bearer(&mut self, net: &Net) -> Result<String> {
        if let Some((token, until)) = &self.access
            && Instant::now() < *until
        {
            return Ok(token.clone());
        }
        let tokens = refresh(&self.client, net, &self.refresh_token)?;
        if let Some(rotated) = tokens.refresh_token.filter(|t| !t.is_empty()) {
            self.refresh_token = rotated;
        }
        // a minute early; a hostile lifetime can't overflow the clock
        let life = tokens.expires_in.unwrap_or(3600).min(86_400).saturating_sub(60);
        let until = Instant::now().checked_add(Duration::from_secs(life)).unwrap_or_else(Instant::now);
        self.access = Some((tokens.access_token.clone(), until));
        Ok(tokens.access_token)
    }

    /// Forget the access token (the server rejected it); the next [`TokenSource::bearer`]
    /// fetches a new one.
    pub fn invalidate(&mut self) {
        self.access = None;
    }
}

/// The PKCE `code_challenge` for a verifier (method `S256`).
pub fn pkce_challenge(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

/// A sign-in in progress: a loopback listener waiting for the browser's redirect.
pub struct LoopbackFlow {
    client: OAuthClient,
    listener: TcpListener,
    redirect_uri: String,
    verifier: String,
    state: String,
    authorize_url: String,
}

impl std::fmt::Debug for LoopbackFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoopbackFlow").field("client", &self.client).field("redirect_uri", &self.redirect_uri).finish_non_exhaustive()
    }
}

impl LoopbackFlow {
    /// Listen on `127.0.0.1:<any port>` and build the sign-in URL.
    pub fn start(client: OAuthClient, rng: &mut dyn Random) -> Result<LoopbackFlow> {
        Url::parse(&client.auth_url).map_err(|e| CloudError::InvalidName(format!("sign-in URL: {e}")))?;
        Url::parse(&client.token_url).map_err(|e| CloudError::InvalidName(format!("token URL: {e}")))?;
        if client.client_id.is_empty() {
            return Err(CloudError::InvalidName("an OAuth client id is needed".into()));
        }
        let mut secret = [0u8; 32];
        rng.fill(&mut secret)?;
        let verifier = base64url(&secret); // 43 characters, the RFC's minimum length
        let mut nonce = [0u8; 16];
        rng.fill(&mut nonce)?;
        let state = base64url(&nonce);
        let listener = TcpListener::bind(("127.0.0.1", client.redirect_port.unwrap_or(0))).map_err(|e| match client.redirect_port {
            Some(port) => CloudError::Io(format!("can't listen for the sign-in on port {port} (in use by another program?): {e}")),
            None => e.into(),
        })?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}");
        let scope = client.scopes.join(" ");
        let challenge = pkce_challenge(&verifier);
        let mut params: Vec<(&str, &str)> = vec![
            ("client_id", &client.client_id),
            ("redirect_uri", &redirect_uri),
            ("response_type", "code"),
            ("scope", &scope),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
        ];
        params.extend(client.extra_auth_params.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let sep = if client.auth_url.contains('?') { '&' } else { '?' };
        let authorize_url = format!("{}{sep}{}", client.auth_url, form(&params));
        Ok(LoopbackFlow { client, listener, redirect_uri, verifier, state, authorize_url })
    }

    /// The URL to open in the user's browser.
    pub fn authorize_url(&self) -> &str {
        &self.authorize_url
    }

    /// Where the provider sends the browser back to (`http://127.0.0.1:<port>`).
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Wait up to `timeout` for the redirect (stopping early when `net.cancel` is raised), then
    /// trade the code for tokens. Requests without this flow's `state` are answered and ignored,
    /// so another program on this computer can't end or hijack the sign-in.
    pub fn wait(self, net: &Net, timeout: Duration) -> Result<Tokens> {
        let deadline = Instant::now() + timeout;
        let mut connections = 0usize;
        loop {
            if net.cancel.load(Ordering::Relaxed) {
                return Err(CloudError::Io("sign-in cancelled".into()));
            }
            if Instant::now() >= deadline {
                return Err(CloudError::Io("sign-in timed out: no answer from the browser".into()));
            }
            let stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            connections += 1;
            if connections > MAX_REDIRECT_CONNECTIONS {
                return Err(CloudError::Io("sign-in stopped: too many connections to the sign-in port".into()));
            }
            match self.answer(stream) {
                Redirect::Code(code) => return exchange_code(&self.client, net, &code, &self.verifier, &self.redirect_uri),
                Redirect::Refused(why) => return Err(CloudError::Denied(format!("sign-in refused: {why}"))),
                Redirect::Ignored => {}
            }
        }
    }

    /// Read one request from the browser and answer it.
    fn answer(&self, mut stream: TcpStream) -> Redirect {
        let outcome = read_request_target(&mut stream).map(|target| self.judge(&target)).unwrap_or(Redirect::Ignored);
        let (status, text) = match &outcome {
            Redirect::Code(_) => ("200 OK", "Signed in. You can close this window and return to LightCraft."),
            Redirect::Refused(_) => ("200 OK", "Sign-in was cancelled. You can close this window."),
            Redirect::Ignored => ("404 Not Found", "Not found."),
        };
        let body = format!("<!doctype html><meta charset=\"utf-8\"><title>LightCraft</title><p>{text}</p>");
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        // the browser may already have gone; the outcome stands either way
        let _ = stream.write_all(reply.as_bytes());
        let _ = stream.flush();
        outcome
    }

    fn judge(&self, target: &str) -> Redirect {
        let Some(query) = target.strip_prefix("/?").or_else(|| target.strip_prefix("?")) else { return Redirect::Ignored };
        let params = parse_form(query);
        let get = |name: &str| params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        if get("state") != Some(self.state.as_str()) {
            return Redirect::Ignored;
        }
        if let Some(error) = get("error") {
            let error: String = error.chars().filter(|c| !c.is_control()).take(200).collect();
            return Redirect::Refused(error);
        }
        match get("code") {
            Some(code) if !code.is_empty() && code.len() <= 4096 => Redirect::Code(code.to_string()),
            _ => Redirect::Ignored,
        }
    }
}

enum Redirect {
    Code(String),
    Refused(String),
    Ignored,
}

/// The request target (`/?code=…`) of a `GET` on the loopback port.
fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(chunk.get(..n)?);
        if buf.len() > MAX_REDIRECT_REQUEST {
            return None;
        }
    }
    let text = std::str::from_utf8(&buf).ok()?;
    let mut parts = text.lines().next()?.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    parts.next().map(str::to_string)
}

/// Trade an authorization code for tokens.
pub fn exchange_code(client: &OAuthClient, net: &Net, code: &str, verifier: &str, redirect_uri: &str) -> Result<Tokens> {
    let mut pairs = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
        ("client_id", client.client_id.as_str()),
    ];
    if let Some(s) = &client.client_secret {
        pairs.push(("client_secret", s));
    }
    let tokens = token_request(client, net, &pairs)?;
    if tokens.refresh_token.is_none() {
        return Err(CloudError::Denied("the provider gave no refresh token, so LightCraft couldn't stay signed in".into()));
    }
    Ok(tokens)
}

/// Tokens for a username and password: the OAuth 2.0 password grant (RFC 6749 §4.3), only for
/// providers that offer nothing else (OpenDrive). The password is sent once and never kept.
pub fn password_grant(client: &OAuthClient, net: &Net, username: &str, password: &str) -> Result<Tokens> {
    let pairs = [("grant_type", "password"), ("client_id", client.client_id.as_str()), ("username", username), ("password", password)];
    let tokens = token_request(client, net, &pairs)?;
    if tokens.refresh_token.as_deref().is_none_or(str::is_empty) {
        return Err(CloudError::Denied("the provider gave no refresh token, so LightCraft couldn't stay signed in".into()));
    }
    Ok(tokens)
}

/// A new access token from a refresh token.
pub fn refresh(client: &OAuthClient, net: &Net, refresh_token: &str) -> Result<Tokens> {
    let mut pairs = vec![("grant_type", "refresh_token"), ("refresh_token", refresh_token), ("client_id", client.client_id.as_str())];
    if let Some(s) = &client.client_secret {
        pairs.push(("client_secret", s));
    }
    token_request(client, net, &pairs)
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

fn token_request(client: &OAuthClient, net: &Net, pairs: &[(&str, &str)]) -> Result<Tokens> {
    let url = Url::parse(&client.token_url).map_err(|e| CloudError::InvalidName(format!("token URL: {e}")))?;
    if !url.tls && !crate::net::is_local(&url.host) {
        return Err(CloudError::Denied("refusing to send credentials over plain http:// to another computer".into()));
    }
    let headers = [("Content-Type", "application/x-www-form-urlencoded".to_string()), ("Accept", "application/json".to_string())];
    let mut resp = net.send("POST", &url, &headers, form(pairs).as_bytes())?;
    let status = resp.status;
    let body = net.body(&mut resp, MAX_META_BODY)?;
    if status == 200 {
        let tokens: Tokens = serde_json::from_slice(&body).map_err(|e| CloudError::Corrupt(format!("token reply: {e}")))?;
        if tokens.access_token.is_empty() {
            return Err(CloudError::Corrupt("token reply without an access token".into()));
        }
        return Ok(tokens);
    }
    let detail = serde_json::from_slice::<TokenError>(&body)
        .map(|e| match e.error_description {
            Some(d) => format!("{} ({})", e.error, d.chars().take(200).collect::<String>()),
            None => e.error,
        })
        .unwrap_or_else(|_| format!("HTTP {status}"));
    match status {
        // invalid_grant: the refresh token was revoked or expired; the user must sign in again
        400 | 401 | 403 => Err(CloudError::Denied(format!("sign in again: {detail}"))),
        _ => Err(CloudError::Io(format!("token request failed: {detail}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::TestRandom;

    #[test]
    fn rfc7636_appendix_b() {
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn sign_in_url_and_redirect_checks() {
        let client = OAuthClient::google_drive("id.apps.googleusercontent.com", Some("not-secret".into()));
        assert!(!format!("{client:?}").contains("not-secret"));
        let flow = LoopbackFlow::start(client, &mut TestRandom(7)).unwrap();
        let url = flow.authorize_url();
        assert!(url.starts_with(
            "https://accounts.google.com/o/oauth2/v2/auth?client_id=id.apps.googleusercontent.com&redirect_uri=http%3A%2F%2F127.0.0.1%3A"
        ));
        assert!(url.contains("&code_challenge_method=S256&"));
        assert!(url.contains(&format!("&code_challenge={}&", pkce_challenge(&flow.verifier))));
        assert!(url.contains("scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdrive.file"));
        assert!(url.ends_with("&access_type=offline&prompt=consent%20select_account"));
        assert_eq!(flow.verifier.len(), 43);

        let state = flow.state.clone();
        assert!(matches!(flow.judge(&format!("/?state={state}&code=4%2F0Ab")), Redirect::Code(c) if c == "4/0Ab"));
        assert!(matches!(flow.judge("/?state=other&code=x"), Redirect::Ignored));
        assert!(matches!(flow.judge("/?code=x"), Redirect::Ignored));
        assert!(matches!(flow.judge("/favicon.ico"), Redirect::Ignored));
        assert!(matches!(flow.judge(&format!("/?state={state}")), Redirect::Ignored));
        assert!(matches!(flow.judge(&format!("/?state={state}&error=access_denied")), Redirect::Refused(e) if e == "access_denied"));
    }

    #[test]
    fn a_client_id_is_required() {
        assert!(LoopbackFlow::start(OAuthClient::google_drive("", None), &mut TestRandom(1)).is_err());
    }
}
