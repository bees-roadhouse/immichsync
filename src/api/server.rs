use serde::Deserialize;
use tracing::{debug, instrument};

use super::{ApiError, ImmichClient};

// The ping endpoint returns `{ "res": "pong" }`.
#[derive(Debug, Deserialize)]
struct PingResponse {
    res: String,
}

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
}
