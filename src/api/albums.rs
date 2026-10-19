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

/// What `PUT /api/albums/{id}/assets` did, per asset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddAssetsOutcome {
    /// Asset ids now in the album because of this call.
    pub added: Vec<String>,
    /// `(asset id, reason)` for each asset the server refused, e.g.
    /// `duplicate` (already in the album) or `no_permission`.
    pub failed: Vec<(String, String)>,
}

/// Parse the add-to-album response. Immich returns `BulkIdResponseDto[]`:
/// `[{"id": "...", "success": true}, {"id": "...", "success": false, "error": "duplicate"}]`
/// (#71: this was parsed as `{successfulIds, failedIds}` and always read as
/// "added 0").
pub(crate) fn parse_add_assets_response(body: &str) -> Result<AddAssetsOutcome, serde_json::Error> {
    #[derive(Deserialize)]
    struct BulkIdResponse {
        id: String,
        success: bool,
        #[serde(default)]
        error: Option<String>,
    }

    let rows: Vec<BulkIdResponse> = serde_json::from_str(body)?;
    let mut out = AddAssetsOutcome::default();
    for r in rows {
        if r.success {
            out.added.push(r.id);
        } else {
            out.failed
                .push((r.id, r.error.unwrap_or_else(|| "unknown".to_string())));
        }
    }
    Ok(out)
}

/// Whether an album call failed because the album no longer exists (or this
/// key can't see it). Immich answers 404, or 400 "Not found or no
/// album.read access"; either way a mirrored album must be recreated.
pub fn album_is_gone(err: &ApiError) -> bool {
    match err {
        ApiError::NotFound(_) => true,
        ApiError::BadRequest(body) => body.to_ascii_lowercase().contains("not found"),
        _ => false,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RenameAlbumRequest<'a> {
    album_name: &'a str,
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
    ) -> Result<AddAssetsOutcome, ApiError> {
        if asset_ids.is_empty() {
            return Ok(AddAssetsOutcome::default());
        }

        debug!("adding {} assets to album", asset_ids.len());

        let body = AddAssetsRequest { ids: asset_ids };
        let url = self.url(&format!("/api/albums/{album_id}/assets"));

        let response = self.client.put(&url).json(&body).send().await?;

        if !response.status().is_success() {
            return Err(Self::map_status_error(response).await);
        }

        // Per-asset results: a refusal (already a member, no permission) is
        // not an error for the call, but it is logged with its reason.
        let body = response.text().await?;
        let outcome = match parse_add_assets_response(&body) {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(error = %e, "unexpected add-to-album response; treating as sent");
                AddAssetsOutcome::default()
            }
        };
        for (id, reason) in &outcome.failed {
            if reason == "duplicate" {
                debug!(asset_id = %id, "asset already in album {album_id}");
            } else {
                tracing::warn!(asset_id = %id, %reason, "asset not added to album {album_id}");
            }
        }
        debug!(
            added = outcome.added.len(),
            failed = outcome.failed.len(),
            "assets added to album"
        );

        Ok(outcome)
    }

    /// Rename an album (`PATCH /api/albums/{id}`).
    #[instrument(skip(self), fields(album_id = album_id, name = name))]
    pub async fn rename_album(&self, album_id: &str, name: &str) -> Result<(), ApiError> {
        let response = self
            .client
            .patch(self.url(&format!("/api/albums/{album_id}")))
            .json(&RenameAlbumRequest { album_name: name })
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Self::map_status_error(response).await);
        }
        debug!("album renamed");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The `BulkIdResponseDto[]` shape #71 documents, for a PUT that added
    /// one asset and found the other already in the album (ids illustrative,
    /// not a live capture).
    const CAPTURED: &str = r#"[{"id":"6d95e8fd-8846-4e69-b35f-7b25669ee105","success":true},{"id":"c39f1ac3-94e5-46dc-8814-5680e70f1788","success":false,"error":"duplicate"}]"#;

    #[test]
    fn a_missing_album_is_recognised_from_404_or_immichs_400() {
        assert!(album_is_gone(&ApiError::NotFound("x".into())));
        assert!(album_is_gone(&ApiError::BadRequest(
            r#"{"message":"Not found or no album.read access","statusCode":400}"#.into()
        )));
        assert!(!album_is_gone(&ApiError::BadRequest(
            "ids must be an array".into()
        )));
        assert!(!album_is_gone(&ApiError::RateLimit));
    }

    #[test]
    fn add_assets_response_reads_bulk_id_results() {
        let out = parse_add_assets_response(CAPTURED).unwrap();
        assert_eq!(
            out.added,
            vec!["6d95e8fd-8846-4e69-b35f-7b25669ee105".to_string()]
        );
        assert_eq!(
            out.failed,
            vec![(
                "c39f1ac3-94e5-46dc-8814-5680e70f1788".to_string(),
                "duplicate".to_string()
            )]
        );
    }

    #[test]
    fn add_assets_response_without_error_text_still_counts_as_failed() {
        let out = parse_add_assets_response(r#"[{"id":"a","success":false}]"#).unwrap();
        assert_eq!(out.failed, vec![("a".to_string(), "unknown".to_string())]);
        assert!(parse_add_assets_response(r#"{"successfulIds":[]}"#).is_err());
    }
}
