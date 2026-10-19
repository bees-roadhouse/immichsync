// Sign in with the server's OAuth (OIDC) and mint a per-device API key (#143).
//
// Flow, all on one worker thread (blocking I/O):
//   1. Bind the loopback listener on 127.0.0.1:CALLBACK_PORT, before anything
//      else, so a port clash fails fast.
//   2. POST /api/oauth/authorize with our redirect, state and PKCE challenge.
//      Immich forwards the redirect to the IdP untouched (spike on #143); the
//      IdP (Keycloak) must list REDIRECT_URI as a valid redirect.
//   3. Open the system browser at the IdP URL; the user signs in (MFA and all).
//   4. The IdP redirects to REDIRECT_URI; the listener takes that one request.
//   5. POST /api/oauth/callback with the full callback URL, state and PKCE
//      verifier. Immich exchanges the code with its own client secret and
//      returns a session token.
//   6. POST /api/api-keys with that token: a key named for this device,
//      scoped to exactly the endpoints ImmichSync calls (ENDPOINTS).
//   7. POST /api/auth/logout, whatever happened in 6, so only the key is left.
//
// The key is returned to the caller, which stores it like a pasted key:
// Keychain on macOS, DPAPI on Windows (config::seal_api_key).

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Deserialize;
use sha2::Digest;
use thiserror::Error;
use tracing::{debug, info, warn};

/// Loopback port for the OAuth redirect. Fixed, because the IdP's allow-list
/// must name it exactly (see #143). 53682 is avoided: rclone's auth uses it.
pub const CALLBACK_PORT: u16 = 47321;
pub const CALLBACK_PATH: &str = "/oauth/callback";

/// The redirect URI for a callback on `port`. The IdP must allow
/// `redirect_for(CALLBACK_PORT)` exactly.
fn redirect_for(port: u16) -> String {
    format!("http://127.0.0.1:{port}{CALLBACK_PATH}")
}

/// Every endpoint ImmichSync calls with its key, and the permission
/// Immich 3.2.2 requires for it (`x-immich-permission` in /api/spec.json).
/// A minted key can't gain permissions later, so this is the whole list;
/// `every_api_call_site_is_in_the_endpoint_table` fails the build's tests
/// when a new call appears in src/api without a row here.
pub const ENDPOINTS: &[(&str, &str, Option<&str>)] = &[
    ("POST", "/api/assets", Some("asset.upload")),
    (
        "POST",
        "/api/assets/bulk-upload-check",
        Some("asset.upload"),
    ),
    ("GET", "/api/assets/{id}", Some("asset.read")), // #142 delete-time check
    ("GET", "/api/albums", Some("album.read")),
    ("POST", "/api/albums", Some("album.create")),
    ("PATCH", "/api/albums/{id}", Some("album.update")), // #132 mirror rename
    ("PUT", "/api/albums/{id}/assets", Some("albumAsset.create")),
    ("GET", "/api/server/ping", None),
    ("GET", "/api/server/about", Some("server.about")), // listed in CLAUDE.md
];

/// What the minted key may do: the permissions `ENDPOINTS` needs, no more,
/// in table order without repeats.
pub fn key_permissions() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for (_, _, p) in ENDPOINTS {
        if let Some(p) = p {
            if !out.contains(p) {
                out.push(p);
            }
        }
    }
    out
}

/// How long to wait for the browser to come back before giving up.
pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Error)]
pub enum OAuthError {
    #[error(
        "port {CALLBACK_PORT} on this computer is in use, so the sign-in can't come back to \
         ImmichSync ({0}). Close whatever is using it and try again."
    )]
    PortInUse(std::io::Error),

    #[error("couldn't open the browser: {0}")]
    Browser(std::io::Error),

    #[error("sign-in timed out; nothing was changed")]
    TimedOut,

    #[error("sign-in cancelled; nothing was changed")]
    Cancelled,

    #[error("the sign-in page reported: {0}")]
    Denied(String),

    #[error("the sign-in response didn't match this request; nothing was changed")]
    StateMismatch,

    #[error("the server rejected the sign-in: {0}")]
    Server(String),

    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("local error: {0}")]
    Io(#[from] std::io::Error),
}

// ─── PKCE + state ───────────────────────────────────────────────────────────

fn random_urlsafe(bytes: usize) -> String {
    // uuid v4 draws from the OS CSPRNG (getrandom); 16 bytes per call.
    let mut buf = Vec::with_capacity(bytes);
    while buf.len() < bytes {
        buf.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    buf.truncate(bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// RFC 7636 S256 challenge for `verifier`.
pub fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier))
}

// ─── Server probing ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Features {
    #[serde(default)]
    oauth: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerConfig {
    #[serde(default)]
    oauth_button_text: String,
}

fn http() -> Result<reqwest::blocking::Client, OAuthError> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(format!("ImmichSync/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()?)
}

fn base(server_url: &str) -> String {
    server_url.trim().trim_end_matches('/').to_string()
}

/// The server's sign-in button text when it offers OAuth, else `None`.
pub fn oauth_button_text(server_url: &str) -> Result<Option<String>, OAuthError> {
    let http = http()?;
    let base = base(server_url);
    let features: Features = http
        .get(format!("{base}/api/server/features"))
        .send()?
        .error_for_status()?
        .json()?;
    if !features.oauth {
        return Ok(None);
    }
    let config: ServerConfig = http
        .get(format!("{base}/api/server/config"))
        .send()?
        .error_for_status()?
        .json()?;
    let text = config.oauth_button_text.trim();
    Ok(Some(if text.is_empty() {
        "Sign in with OAuth".to_string()
    } else {
        text.to_string()
    }))
}

// ─── Loopback callback ──────────────────────────────────────────────────────

/// Bind the callback listener. Loopback only, never 0.0.0.0.
pub fn bind_callback(port: u16) -> Result<TcpListener, OAuthError> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(addr).map_err(OAuthError::PortInUse)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

const PAGE_OK: &str = "<!doctype html><meta charset=utf-8><title>ImmichSync</title>\
<body style=\"font:16px system-ui;margin:3em\"><h2>Signed in</h2>\
<p>You can close this tab and go back to ImmichSync.</p>";
const PAGE_FAIL: &str = "<!doctype html><meta charset=utf-8><title>ImmichSync</title>\
<body style=\"font:16px system-ui;margin:3em\"><h2>Sign-in didn't complete</h2>\
<p>Go back to ImmichSync for details.</p>";

/// Wait for the browser's redirect to the callback path and return the full
/// callback URL (what `/api/oauth/callback` wants). Other paths (a favicon
/// probe) get a 404 and the wait goes on. Checks `cancel` and `timeout`.
pub fn wait_for_callback(
    listener: &TcpListener,
    expected_state: &str,
    cancel: &AtomicBool,
    timeout: Duration,
) -> Result<String, OAuthError> {
    let port = listener.local_addr()?.port();
    let deadline = Instant::now() + timeout;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(OAuthError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(OAuthError::TimedOut);
        }
        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if let Some(result) = handle_callback_request(stream, port, expected_state)? {
            return result;
        }
    }
}

/// One HTTP request on the listener. `Ok(None)` means "not ours, keep
/// waiting"; `Ok(Some(..))` ends the wait.
fn handle_callback_request(
    mut stream: TcpStream,
    port: u16,
    expected_state: &str,
) -> Result<Option<Result<String, OAuthError>>, OAuthError> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    // "GET /oauth/callback?code=…&state=… HTTP/1.1"
    let target = line.split_whitespace().nth(1).unwrap_or("");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != CALLBACK_PATH {
        respond(&mut stream, "404 Not Found", "")?;
        return Ok(None);
    }
    let params: Vec<(String, String)> = reqwest::Url::parse(&format!("http://127.0.0.1/?{query}"))
        .map(|u| u.query_pairs().into_owned().collect())
        .unwrap_or_default();
    let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());

    let outcome = if let Some(err) = get("error") {
        let desc = get("error_description").unwrap_or_default();
        Err(OAuthError::Denied(
            format!("{err} {desc}").trim().to_string(),
        ))
    } else if get("state").as_deref() != Some(expected_state) {
        Err(OAuthError::StateMismatch)
    } else if get("code").is_none() {
        Err(OAuthError::Server("no code in the sign-in response".into()))
    } else {
        Ok(format!("http://127.0.0.1:{port}{target}"))
    };
    let page = if outcome.is_ok() { PAGE_OK } else { PAGE_FAIL };
    respond(&mut stream, "200 OK", page)?;
    Ok(Some(outcome))
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

// ─── Immich calls ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AuthorizeResponse {
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct ApiKeyCreated {
    secret: String,
}

fn server_error(resp: reqwest::blocking::Response) -> OAuthError {
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    OAuthError::Server(format!(
        "HTTP {status}: {}",
        body.chars().take(300).collect::<String>()
    ))
}

/// The IdP URL to open in the browser.
fn authorize(
    http: &reqwest::blocking::Client,
    base: &str,
    redirect: &str,
    state: &str,
    challenge: &str,
) -> Result<String, OAuthError> {
    let resp = http
        .post(format!("{base}/api/oauth/authorize"))
        .json(&serde_json::json!({
            "redirectUri": redirect,
            "state": state,
            "codeChallenge": challenge,
        }))
        .send()?;
    if !resp.status().is_success() {
        return Err(server_error(resp));
    }
    Ok(resp.json::<AuthorizeResponse>()?.url)
}

/// Exchange the callback URL for an Immich session token.
fn exchange(
    http: &reqwest::blocking::Client,
    base: &str,
    callback_url: &str,
    state: &str,
    verifier: &str,
) -> Result<String, OAuthError> {
    let resp = http
        .post(format!("{base}/api/oauth/callback"))
        .json(&serde_json::json!({
            "url": callback_url,
            "state": state,
            "codeVerifier": verifier,
        }))
        .send()?;
    if !resp.status().is_success() {
        return Err(server_error(resp));
    }
    Ok(resp.json::<LoginResponse>()?.access_token)
}

/// Mint the device's API key with the session token.
fn create_api_key(
    http: &reqwest::blocking::Client,
    base: &str,
    token: &str,
    name: &str,
) -> Result<String, OAuthError> {
    let resp = http
        .post(format!("{base}/api/api-keys"))
        .bearer_auth(token)
        .json(&serde_json::json!({ "name": name, "permissions": key_permissions() }))
        .send()?;
    if !resp.status().is_success() {
        return Err(server_error(resp));
    }
    Ok(resp.json::<ApiKeyCreated>()?.secret)
}

/// End the session so only the API key remains. Failure is logged, not
/// fatal: the key is already made, and the session expires on its own.
fn logout(http: &reqwest::blocking::Client, base: &str, token: &str) {
    match http
        .post(format!("{base}/api/auth/logout"))
        .bearer_auth(token)
        .send()
    {
        Ok(r) if r.status().is_success() => debug!("Sign-in session logged out"),
        Ok(r) => warn!(status = %r.status(), "Logging out the sign-in session failed"),
        Err(e) => warn!(error = %e, "Logging out the sign-in session failed"),
    }
}

/// The key's name in Immich's API Keys list: one per device, revocable.
pub fn key_name() -> String {
    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "this computer".into());
    format!("ImmichSync on {host}")
}

/// The whole flow (see the module header). `open_browser` opens the IdP URL;
/// `cancel` aborts the wait. Returns the new API key's secret.
pub fn sign_in_and_create_key(
    server_url: &str,
    open_browser: impl FnOnce(&str) -> std::io::Result<()>,
    cancel: &AtomicBool,
    timeout: Duration,
) -> Result<String, OAuthError> {
    sign_in_with(server_url, CALLBACK_PORT, open_browser, cancel, timeout)
}

fn sign_in_with(
    server_url: &str,
    port: u16,
    open_browser: impl FnOnce(&str) -> std::io::Result<()>,
    cancel: &AtomicBool,
    timeout: Duration,
) -> Result<String, OAuthError> {
    let http = http()?;
    let base = base(server_url);
    let listener = bind_callback(port)?;
    let redirect = redirect_for(port);
    let state = random_urlsafe(24);
    let verifier = random_urlsafe(48); // 64 chars, inside RFC 7636's 43..128
    let idp_url = authorize(&http, &base, &redirect, &state, &pkce_challenge(&verifier))?;

    info!("Opening the browser for OAuth sign-in");
    open_browser(&idp_url).map_err(OAuthError::Browser)?;
    let callback_url = wait_for_callback(&listener, &state, cancel, timeout)?;
    drop(listener);

    let token = exchange(&http, &base, &callback_url, &state, &verifier)?;
    let key = create_api_key(&http, &base, &token, &key_name());
    logout(&http, &base, &token);
    let key = key?;
    info!("Created this device's API key");
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::{Arc, Mutex};

    #[test]
    fn pkce_matches_the_rfc_7636_example() {
        // RFC 7636 Appendix B (also checked with `openssl dgst -sha256`).
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_values_are_urlsafe_long_and_distinct() {
        let a = random_urlsafe(48);
        assert_eq!(a.len(), 64);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(a, random_urlsafe(48));
    }

    #[test]
    fn the_redirect_is_loopback_on_the_registered_port() {
        assert_eq!(
            redirect_for(CALLBACK_PORT),
            "http://127.0.0.1:47321/oauth/callback"
        );
    }

    #[test]
    fn the_key_gets_each_needed_permission_once() {
        assert_eq!(
            key_permissions(),
            [
                "asset.upload",
                "asset.read",
                "album.read",
                "album.create",
                "album.update",
                "albumAsset.create",
                "server.about"
            ]
        );
    }

    /// Scan the API client's source for request call sites
    /// (`.get(self.url(...`, `.post(...`, ...) and require each method and
    /// path in `ENDPOINTS`, so a new endpoint can't ship without its
    /// permission on the key. `{album_id}`-style placeholders compare as
    /// `{id}`. The sign-in calls in this file use the session token, not the
    /// key, and aren't scanned.
    #[test]
    fn every_api_call_site_is_in_the_endpoint_table() {
        let sources = [
            include_str!("albums.rs"),
            include_str!("assets.rs"),
            include_str!("server.rs"),
            include_str!("mod.rs"),
        ];
        let normalise = |p: &str| {
            let mut out = String::new();
            let mut in_brace = false;
            for c in p.chars() {
                match c {
                    '{' => {
                        in_brace = true;
                        out.push_str("{id}");
                    }
                    '}' => in_brace = false,
                    _ if in_brace => {}
                    _ => out.push(c),
                }
            }
            out
        };
        let mut found = Vec::new();
        for src in sources {
            let flat: String = src.split_whitespace().collect::<Vec<_>>().join("");
            for method in ["get", "post", "put", "patch", "delete"] {
                let needle = format!(".{method}(self.url(");
                let mut rest = flat.as_str();
                while let Some(i) = rest.find(&needle) {
                    rest = &rest[i + needle.len()..];
                    // self.url("/api/..") or self.url(&format!("/api/.."))
                    let Some(q) = rest.find("\"/api/") else { break };
                    let path = &rest[q + 1..];
                    let path = &path[..path.find('"').unwrap()];
                    found.push((method.to_uppercase(), normalise(path)));
                }
            }
            // `let url = self.url(..)` followed by `.put(url)` and the like.
            let mut rest = flat.as_str();
            while let Some(i) = rest.find("leturl=self.url(") {
                rest = &rest[i..];
                let q = rest.find("\"/api/").unwrap();
                let path = &rest[q + 1..];
                let path = normalise(&path[..path.find('"').unwrap()]);
                let after = &rest[q..];
                let method = ["get", "post", "put", "patch", "delete"]
                    .into_iter()
                    .filter_map(|m| {
                        [format!(".{m}(url"), format!(".{m}(&url")]
                            .iter()
                            .filter_map(|n| after.find(n.as_str()))
                            .min()
                            .map(|at| (at, m))
                    })
                    .min()
                    .map(|(_, m)| m.to_uppercase())
                    .expect("a request on the url");
                found.push((method, path));
                rest = &rest[1..];
            }
        }
        assert!(found.len() >= 6, "the scan found too little: {found:?}");
        for (method, path) in &found {
            assert!(
                ENDPOINTS.iter().any(|(m, p, _)| m == method && p == path),
                "{method} {path} is called but not in ENDPOINTS (give the key its permission)"
            );
        }
    }

    #[test]
    fn the_key_asks_only_for_what_the_app_calls() {
        let perms = key_permissions();
        assert!(!perms.contains(&"all"));
        assert!(!perms.iter().any(|p| p.ends_with(".delete")));
        assert!(perms.contains(&"asset.upload"));
    }

    fn get(port: u16, target: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    fn listen() -> (TcpListener, u16) {
        let l = bind_callback(0).unwrap();
        let port = l.local_addr().unwrap().port();
        (l, port)
    }

    #[test]
    fn the_callback_returns_the_full_url_and_ignores_other_paths() {
        let (l, port) = listen();
        let t = std::thread::spawn(move || {
            let favicon = get(port, "/favicon.ico");
            let page = get(port, "/oauth/callback?code=abc&state=s1");
            (favicon, page)
        });
        let url =
            wait_for_callback(&l, "s1", &AtomicBool::new(false), Duration::from_secs(5)).unwrap();
        let (favicon, page) = t.join().unwrap();
        assert!(favicon.starts_with("HTTP/1.1 404"));
        assert!(page.contains("Signed in"));
        assert_eq!(
            url,
            format!("http://127.0.0.1:{port}/oauth/callback?code=abc&state=s1")
        );
    }

    #[test]
    fn a_wrong_state_or_an_idp_error_ends_the_wait_with_an_error() {
        let (l, port) = listen();
        let t = std::thread::spawn(move || get(port, "/oauth/callback?code=abc&state=evil"));
        let r = wait_for_callback(&l, "s1", &AtomicBool::new(false), Duration::from_secs(5));
        assert!(matches!(r, Err(OAuthError::StateMismatch)), "{r:?}");
        assert!(t.join().unwrap().contains("didn't complete"));

        let (l, port) = listen();
        let t = std::thread::spawn(move || {
            get(
                port,
                "/oauth/callback?error=access_denied&error_description=User+cancelled",
            )
        });
        let r = wait_for_callback(&l, "s1", &AtomicBool::new(false), Duration::from_secs(5));
        assert!(
            matches!(&r, Err(OAuthError::Denied(m)) if m.contains("User cancelled")),
            "{r:?}"
        );
        t.join().unwrap();
    }

    #[test]
    fn the_wait_stops_on_cancel_and_on_timeout() {
        let (l, _) = listen();
        let r = wait_for_callback(&l, "s", &AtomicBool::new(true), Duration::from_secs(5));
        assert!(matches!(r, Err(OAuthError::Cancelled)));
        let r = wait_for_callback(&l, "s", &AtomicBool::new(false), Duration::from_millis(150));
        assert!(matches!(r, Err(OAuthError::TimedOut)));
    }

    #[test]
    fn a_taken_port_is_reported_as_such() {
        let (_held, port) = listen();
        assert!(matches!(bind_callback(port), Err(OAuthError::PortInUse(_))));
    }

    // ── End to end against a fake Immich ────────────────────────────────

    /// A minimal HTTP/1.1 server answering Immich's four endpoints, and
    /// recording (path, authorization, body) per request.
    struct FakeImmich {
        base: String,
        log: Arc<Mutex<Vec<(String, String, String)>>>,
    }

    fn fake_immich(api_key_status: u16) -> FakeImmich {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let log2 = log.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let log = log2.clone();
                std::thread::spawn(move || serve(stream, log, api_key_status));
            }
        });
        FakeImmich { base, log }
    }

    fn serve(mut s: TcpStream, log: Arc<Mutex<Vec<(String, String, String)>>>, key_status: u16) {
        let mut reader = BufReader::new(s.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
        let (mut len, mut auth) = (0usize, String::new());
        loop {
            let mut h = String::new();
            reader.read_line(&mut h).unwrap();
            if h == "\r\n" || h.is_empty() {
                break;
            }
            let lower = h.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
            if lower.starts_with("authorization:") {
                auth = h["authorization:".len()..].trim().to_string();
            }
        }
        let mut body = vec![0; len];
        reader.read_exact(&mut body).unwrap();
        let body = String::from_utf8(body).unwrap();
        log.lock().unwrap().push((path.clone(), auth, body.clone()));

        let (status, reply) = match path.as_str() {
            "/api/oauth/authorize" => {
                let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                // The "IdP" is the redirect itself, with a code.
                let url = format!(
                    "{}?code=CODE&state={}",
                    v["redirectUri"].as_str().unwrap(),
                    v["state"].as_str().unwrap()
                );
                (201, serde_json::json!({ "url": url }).to_string())
            }
            "/api/oauth/callback" => (201, r#"{"accessToken":"SESSION"}"#.to_string()),
            "/api/api-keys" if key_status == 201 => (201, r#"{"secret":"THE-KEY"}"#.to_string()),
            "/api/api-keys" => (key_status, r#"{"message":"nope"}"#.to_string()),
            "/api/auth/logout" => (200, r#"{"successful":true}"#.to_string()),
            _ => (404, "{}".to_string()),
        };
        let _ = write!(
            s,
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
    }

    /// The "browser": follow the IdP URL straight to the loopback callback.
    fn fake_browser(url: &str) -> std::io::Result<()> {
        let url = url.to_string();
        std::thread::spawn(move || {
            let rest = url.strip_prefix("http://127.0.0.1:").unwrap();
            let (port, target) = rest.split_once('/').unwrap();
            get(port.parse().unwrap(), &format!("/{target}"));
        });
        Ok(())
    }

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn sign_in_mints_a_scoped_key_and_logs_the_session_out() {
        let immich = fake_immich(201);
        let key = sign_in_with(
            &format!("{}/", immich.base),
            free_port(),
            fake_browser,
            &AtomicBool::new(false),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(key, "THE-KEY");

        let log = immich.log.lock().unwrap();
        let paths: Vec<&str> = log.iter().map(|(p, _, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/api/oauth/authorize",
                "/api/oauth/callback",
                "/api/api-keys",
                "/api/auth/logout"
            ]
        );
        let authorize: serde_json::Value = serde_json::from_str(&log[0].2).unwrap();
        let callback: serde_json::Value = serde_json::from_str(&log[1].2).unwrap();
        assert_eq!(
            pkce_challenge(callback["codeVerifier"].as_str().unwrap()),
            authorize["codeChallenge"].as_str().unwrap(),
            "the verifier sent on callback matches the challenge sent on authorize"
        );
        assert_eq!(callback["state"], authorize["state"]);
        assert!(callback["url"].as_str().unwrap().contains("code=CODE"));
        let key_req: serde_json::Value = serde_json::from_str(&log[2].2).unwrap();
        assert_eq!(log[2].1, "Bearer SESSION");
        assert!(key_req["name"]
            .as_str()
            .unwrap()
            .starts_with("ImmichSync on "));
        assert_eq!(
            key_req["permissions"].as_array().unwrap().len(),
            key_permissions().len()
        );
        assert_eq!(log[3].1, "Bearer SESSION");
    }

    #[test]
    fn a_failed_key_creation_still_logs_the_session_out() {
        let immich = fake_immich(403);
        let r = sign_in_with(
            &immich.base,
            free_port(),
            fake_browser,
            &AtomicBool::new(false),
            Duration::from_secs(10),
        );
        assert!(
            matches!(&r, Err(OAuthError::Server(m)) if m.contains("403")),
            "{r:?}"
        );
        let log = immich.log.lock().unwrap();
        assert_eq!(log.last().unwrap().0, "/api/auth/logout");
    }
}
