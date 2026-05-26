pub mod albums;
pub mod assets;
pub mod server;

use reqwest::header::{HeaderMap, HeaderValue};
use std::time::Duration;
use thiserror::Error;

/// All errors that can arise from Immich API calls.
#[derive(Debug, Error)]
pub enum ApiError {
    /// 401 Unauthorized — bad or missing API key.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// 404 Not Found.
    #[error("resource not found: {0}")]
    NotFound(String),

    /// 5xx Server Error.
    #[error("server error: {0}")]
    ServerError(String),

    /// Network-level failure (DNS, timeout, TLS, etc.).
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    /// 429 Too Many Requests.
    #[error("rate limited by server")]
    RateLimit,

    /// 400 Bad Request.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Any other unexpected status or response.
    #[error("unexpected error: {0}")]
    Unexpected(String),
}

/// Shared HTTP client that carries the base URL and API key for every request.
///
/// The API key is set as a default header on the underlying `reqwest::Client`
/// at construction time, so every request inherits it automatically; we do
/// not need to store the raw key after that point.
#[derive(Clone, Debug)]
pub struct ImmichClient {
    pub(crate) client: reqwest::Client,
    pub(crate) base_url: String,
    /// Bandwidth limit in bytes/sec. 0 means unlimited.
    pub(crate) bandwidth_limit_bps: u64,
}

impl ImmichClient {
    /// Create a new client.
    ///
    /// `base_url` should be the server root without a trailing slash, e.g.
    /// `"https://photos.example.com"`. The `x-api-key` header is set as a
    /// default so every request inherits it automatically.
    ///
    /// Uses a 3600s inactivity (read) timeout; pass a timeout via
    /// [`with_bandwidth_limit`] if you need a different value.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self, ApiError> {
        Self::with_bandwidth_limit(base_url, api_key, 0, 3600)
    }

    /// Create a new client with an optional bandwidth limit (in KB/s, 0 = unlimited)
    /// and a configurable per-request inactivity timeout (in seconds).
    ///
    /// `timeout_secs` is the HTTP read (inactivity) timeout: how long to wait
    /// with no response bytes before aborting. Size it for the slowest backend
    /// you expect — a 50GB upload to an Orange Pi can take several minutes of
    /// post-upload server processing before the first response byte arrives.
    pub fn with_bandwidth_limit(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        bandwidth_limit_kbps: u64,
        timeout_secs: u64,
    ) -> Result<Self, ApiError> {
        let api_key = api_key.into();
        let base_url = base_url.into().trim_end_matches('/').to_string();

        let mut default_headers = HeaderMap::new();
        let key_value = HeaderValue::from_str(&api_key)
            .map_err(|e| ApiError::Unexpected(format!("invalid API key characters: {e}")))?;
        // Every request to Immich requires this header. Setting it as a
        // default means callers never have to remember to add it.
        default_headers.insert("x-api-key", key_value);

        // Timeouts/keep-alive tuned for streaming uploads of large files (50GB+ videos):
        // - No overall .timeout() — a 50GB upload at 10MB/s legitimately takes ~83 minutes.
        //   An overall request timeout would kill a healthy long-running transfer, so we
        //   deliberately omit it. Only inactivity (read_timeout) is bounded.
        // - read_timeout: configurable inactivity timeout (default 3600s). This is NOT a
        //   transfer-duration limit — it fires only when NO response bytes arrive for this
        //   long. A slow backend (Orange Pi / Rockchip SBC) can take many minutes checksumming
        //   a 50GB file after the last upload byte is sent; the read_timeout must outlast that
        //   post-upload processing window. 1 hour of silence = give up.
        // - connect_timeout: 30s to establish TCP+TLS is plenty on any non-pathological link.
        // - tcp_keepalive: 60s probes keep middleboxes (firewalls, NATs) from idle-closing
        //   the connection during long uploads. Without this, a quiet TCP stream is silently
        //   reaped by a firewall and read_timeout takes the full timeout duration to notice.
        // - pool_idle_timeout: 120s caps how long an idle connection stays in the pool
        //   before a fresh one is opened on next use.
        let client = reqwest::Client::builder()
            .default_headers(default_headers)
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(timeout_secs))
            .tcp_keepalive(Duration::from_secs(60))
            .pool_idle_timeout(Duration::from_secs(120))
            .build()
            .map_err(ApiError::Network)?;

        Ok(Self {
            client,
            base_url,
            bandwidth_limit_bps: bandwidth_limit_kbps * 1024,
        })
    }

    /// Construct a full URL from a path fragment.
    ///
    /// `path` must begin with `/`, e.g. `"/api/server/ping"`.
    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Map an HTTP status code to the appropriate `ApiError` variant, using the
    /// response body (if readable) as the detail message.
    pub(crate) async fn map_status_error(response: reqwest::Response) -> ApiError {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable body>".to_string());

        match status.as_u16() {
            400 => ApiError::BadRequest(body),
            401 => ApiError::Auth(body),
            404 => ApiError::NotFound(body),
            429 => ApiError::RateLimit,
            500..=599 => ApiError::ServerError(format!("HTTP {status}: {body}")),
            other => ApiError::Unexpected(format!("HTTP {other}: {body}")),
        }
    }
}
