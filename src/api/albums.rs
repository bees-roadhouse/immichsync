use serde::{Deserialize, Serialize};
use tracing::{debug, instrument};

use super::{ApiError, ImmichClient};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// An Immich album, as returned by the API.
///
/// Only the fields the rest of the app consumes are deserialized; extra JSON
/// keys returned by the server (asset_count, description, owner_id, etc.)
/// are silently ignored by serde.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Album {
    /// Immich UUID for the album.
    pub id: String,

    /// Human-readable album name.
    pub album_name: String,
}

// ---------------------------------------------------------------------------
// Internal request / response shapes
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateAlbumRequest<'a> {
    album_name: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AddAssetsRequest {
    ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddAssetsResponse {
    /// Number of assets that were successfully added.
    #[allow(dead_code)]
    #[serde(default)]
    successful_ids: Vec<String>,

    /// Asset IDs that were rejected (e.g. already in the album).
    #[allow(dead_code)]
    #[serde(default)]
    failed_ids: Vec<String>,
}

// ---------------------------------------------------------------------------
// ImmichClient methods
// ---------------------------------------------------------------------------

impl ImmichClient {
    /// Create a new album with the given name.
    ///
    /// Returns the newly created `Album` including its server-assigned UUID.
    /// If an album with the same name already exists, Immich still creates a
    /// new one (album names are not unique); callers that want "get or create"
    /// semantics should call `get_albums()` first.
    #[instrument(skip(self), fields(name = name))]
    pub async fn create_album(&self, name: &str) -> Result<Album, ApiError> {
        debug!("creating album");

        let body = CreateAlbumRequest { album_name: name };

        let response = self
            .client
            .post(self.url("/api/albums"))
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::map_status_error(response).await);
        }

        let album: Album = response.json().await?;
        debug!(album_id = %album.id, "album created");
        Ok(album)
    }

    /// Add one or more assets to an existing album.
    ///
    /// Immich silently ignores asset IDs that are already members of the
    /// album, so this is safe to call idempotently.
    #[instrument(skip(self, asset_ids), fields(album_id = album_id, count = asset_ids.len()))]
    pub async fn add_assets_to_album(
        &self,
        album_id: &str,
        asset_ids: Vec<String>,
    ) -> Result<(), ApiError> {
        if asset_ids.is_empty() {
            return Ok(());
        }

        debug!("adding {} assets to album", asset_ids.len());

        let body = AddAssetsRequest { ids: asset_ids };
        let url = self.url(&format!("/api/albums/{album_id}/assets"));

        let response = self.client.put(&url).json(&body).send().await?;

        if !response.status().is_success() {
            return Err(Self::map_status_error(response).await);
        }

        // Parse the response to log any failures (non-fatal; we still consider
        // the call a success because partial adds are not an error condition).
        let add_response: AddAssetsResponse = response.json().await.unwrap_or(AddAssetsResponse {
            successful_ids: vec![],
            failed_ids: vec![],
        });

        if !add_response.failed_ids.is_empty() {
            tracing::warn!(
                failed = ?add_response.failed_ids,
                "some assets could not be added to album {album_id}"
            );
        }

        debug!(
            added = add_response.successful_ids.len(),
            "assets added to album"
        );

        Ok(())
    }

    /// List all albums visible to the authenticated user.
    #[instrument(skip(self))]
    pub async fn get_albums(&self) -> Result<Vec<Album>, ApiError> {
        debug!("fetching album list");

        let response = self.client.get(self.url("/api/albums")).send().await?;

        if !response.status().is_success() {
            return Err(Self::map_status_error(response).await);
        }

        let albums: Vec<Album> = response.json().await?;
        debug!("fetched {} albums", albums.len());
        Ok(albums)
    }
}
