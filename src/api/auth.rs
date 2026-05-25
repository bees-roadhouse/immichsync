use serde::Deserialize;
use tracing::{debug, instrument};

use super::{ApiError, ImmichClient};

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

/// The authenticated user, returned by `/api/users/me`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInfo {
    /// Immich-internal UUID for this user.
    pub id: String,

    /// User's email address.
    pub email: String,

    /// Display name (first + last name concatenated by Immich).
    pub name: String,

    /// Whether this user has administrator privileges.
    #[serde(default)]
    pub is_admin: bool,

    /// Storage quota in bytes, if a quota is set.
    #[serde(default)]
    pub quota_size_in_bytes: Option<i64>,

    /// Bytes currently used against the quota.
    #[serde(default)]
    pub quota_usage_in_bytes: Option<i64>,
}

// ---------------------------------------------------------------------------
// ImmichClient methods
// ---------------------------------------------------------------------------

impl ImmichClient {
    /// Validate the configured API key by fetching the current user's profile.
    ///
    /// A successful response means the key is valid and the server is reachable.
    /// An `ApiError::Auth` is returned for a 401, which means the key is wrong
    /// or has been revoked.
    #[instrument(skip(self), fields(url = %self.url("/api/users/me")))]
    pub async fn validate_api_key(&self) -> Result<UserInfo, ApiError> {
        debug!("validating API key via /api/users/me");

        let response = self.client.get(self.url("/api/users/me")).send().await?;

        if !response.status().is_success() {
            return Err(Self::map_status_error(response).await);
        }

        let user: UserInfo = response.json().await?;
        debug!(user_id = %user.id, email = %user.email, "API key valid");
        Ok(user)
    }
}
