use serde::Deserialize;
use tracing::{debug, instrument};

use super::{ApiError, ImmichClient};

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

/// Subset of the `/api/server/about` response that we care about.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    /// Immich server version string, e.g. `"1.105.1"`.
    pub version: String,

    /// Whether the server has completed its initial setup.
    #[serde(default)]
    pub licensed: bool,
}

// The ping endpoint returns `{ "res": "pong" }`.
#[derive(Debug, Deserialize)]
struct PingResponse {
    res: String,
}

// ---------------------------------------------------------------------------
// ImmichClient methods
// ---------------------------------------------------------------------------

impl ImmichClient {
    /// Send a health-check ping to the server.
    ///
    /// Returns `true` when the server responds with the expected `"pong"` value.
    /// Returns `false` if the response body is unexpected (server is alive but
    /// not an Immich instance). All network and HTTP errors propagate normally.
    ///
    /// Note: `/api/server/ping` is a public endpoint — no auth required.
    #[instrument(skip(self), fields(url = %self.url("/api/server/ping")))]
    pub async fn ping(&self) -> Result<bool, ApiError> {
        debug!("pinging Immich server");

        let response = self.client.get(self.url("/api/server/ping")).send().await?;

        if !response.status().is_success() {
            return Err(Self::map_status_error(response).await);
        }

        let ping: PingResponse = response.json().await?;
        Ok(ping.res == "pong")
    }

    /// Retrieve server metadata from `/api/server/about`.
    ///
    /// Requires the `server.about` API key permission.
    #[instrument(skip(self), fields(url = %self.url("/api/server/about")))]
    pub async fn server_info(&self) -> Result<ServerInfo, ApiError> {
        debug!("fetching server info");

        let response = self
            .client
            .get(self.url("/api/server/about"))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::map_status_error(response).await);
        }

        let info: ServerInfo = response.json().await?;
        debug!(version = %info.version, "server info received");
        Ok(info)
    }
}
