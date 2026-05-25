use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tokio::fs;
use tokio_util::io::ReaderStream;
use tracing::{debug, instrument, warn};

use super::{ApiError, ImmichClient};

/// Chunk size for streaming file uploads. 64 KB balances syscall overhead
/// against memory residency — at any moment only one chunk is in flight,
/// so a 4 GB video uses ~64 KB instead of 4 GB.
const UPLOAD_CHUNK_SIZE: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Result of a single asset upload (`POST /api/assets`).
#[derive(Debug, Clone)]
pub struct UploadResult {
    /// The Immich asset UUID assigned to this file.
    pub asset_id: String,

    /// `true` when the server recognised this as a file it already has
    /// (server-side deduplication triggered).
    pub is_duplicate: bool,
}

/// One item sent in a bulk-upload-check request.
#[derive(Debug, Clone, Serialize)]
pub struct BulkCheckItem {
    /// The client-side device asset ID (e.g. `"{path_hash}-{filename}"`).
    pub id: String,

    /// SHA-1 of the file contents, lower-case hex.
    pub checksum: String,
}

/// The server's verdict for a single asset in a bulk-upload-check response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkCheckResult {
    /// The device asset ID that was queried.
    pub id: String,

    /// Immich's verdict: `"accept"` (not a duplicate) or `"reject"` (already
    /// exists).  May be other values in future Immich versions.
    pub action: String,

    /// If `action` is `"reject"`, the existing Immich asset UUID is provided
    /// here so callers can record it without re-uploading.
    #[serde(default)]
    pub asset_id: Option<String>,
}

impl BulkCheckResult {
    /// Returns `true` when Immich says this asset already exists on the server.
    pub fn is_duplicate(&self) -> bool {
        self.action == "reject"
    }
}

// ---------------------------------------------------------------------------
// Internal response shapes
// ---------------------------------------------------------------------------

/// Raw JSON returned by `POST /api/assets`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssetUploadResponse {
    id: String,
}

/// Raw JSON returned by `POST /api/assets/bulk-upload-check`.
#[derive(Debug, Deserialize)]
struct BulkUploadCheckResponse {
    results: Vec<BulkCheckResult>,
}

// ---------------------------------------------------------------------------
// ImmichClient methods
// ---------------------------------------------------------------------------

impl ImmichClient {
    /// Upload a single file to the Immich server.
    ///
    /// # Arguments
    ///
    /// * `file_path`       — Path to the file on disk.
    /// * `device_asset_id` — Stable client-side identifier, e.g. `"{sha1}-{filename}"`.
    /// * `device_id`       — Identifier for this machine, e.g. `"immichsync-{hostname}"`.
    /// * `created_at`      — Photo/video creation timestamp (EXIF or file ctime).
    /// * `modified_at`     — File modification timestamp.
    ///
    /// # Errors
    ///
    /// Returns `ApiError::Auth` for a 401, `ApiError::BadRequest` for a 400,
    /// `ApiError::ServerError` for 5xx responses, and `ApiError::Network` for
    /// transport failures.
    #[instrument(skip(self), fields(path = %file_path.as_ref().display()))]
    pub async fn upload_asset(
        &self,
        file_path: impl AsRef<Path> + std::fmt::Debug,
        device_asset_id: &str,
        device_id: &str,
        created_at: DateTime<Utc>,
        modified_at: DateTime<Utc>,
    ) -> Result<UploadResult, ApiError> {
        let path = file_path.as_ref();

        // Stat the file to get its length for the multipart Content-Length.
        let metadata = fs::metadata(path)
            .await
            .map_err(|e| ApiError::Unexpected(format!("failed to stat file: {e}")))?;
        let file_size = metadata.len();

        // Open the file and wrap it in a chunked stream. At any moment only
        // one chunk is held in memory, regardless of file size.
        let file = fs::File::open(path)
            .await
            .map_err(|e| ApiError::Unexpected(format!("failed to open file: {e}")))?;
        let reader_stream = ReaderStream::with_capacity(file, UPLOAD_CHUNK_SIZE);

        debug!(file_size, "streaming asset upload");

        // Optionally throttle: sleep between chunks proportional to chunk size.
        // Token-bucket would be more accurate but a per-chunk sleep is
        // sufficient for the user-facing "limit upload speed" feature.
        let body = if self.bandwidth_limit_bps > 0 {
            let bps = self.bandwidth_limit_bps;
            let throttled = reader_stream.then(move |chunk| async move {
                if let Ok(ref bytes) = chunk {
                    let sleep_ms = (bytes.len() as u64 * 1000) / bps;
                    if sleep_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                    }
                }
                chunk
            });
            reqwest::Body::wrap_stream(throttled)
        } else {
            reqwest::Body::wrap_stream(reader_stream)
        };

        // Derive a filename for the multipart part; fall back to "asset" if
        // the path has no filename component (should never happen in practice).
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("asset")
            .to_string();

        // Determine MIME type from extension; Immich accepts it but doesn't
        // strictly require a correct value.
        let mime = mime_from_extension(path);

        let file_part = Part::stream_with_length(body, file_size)
            .file_name(filename)
            .mime_str(mime)
            .map_err(|e| ApiError::Unexpected(format!("invalid MIME type: {e}")))?;

        let form = Form::new()
            .part("assetData", file_part)
            .text("deviceAssetId", device_asset_id.to_string())
            .text("deviceId", device_id.to_string())
            .text("fileCreatedAt", created_at.to_rfc3339())
            .text("fileModifiedAt", modified_at.to_rfc3339())
            .text("isFavorite", "false");

        debug!("uploading asset");

        // reqwest's `.multipart()` sets Content-Type to multipart/form-data
        // with the correct boundary, overriding the client's default JSON
        // Content-Type header for this request only.
        let response = self
            .client
            .post(self.url("/api/assets"))
            .multipart(form)
            .send()
            .await?;

        let status = response.status();

        // Immich returns 200 for duplicates and 201 for new uploads.
        let is_duplicate = status.as_u16() == 200;

        if !status.is_success() {
            return Err(Self::map_status_error(response).await);
        }

        let body: AssetUploadResponse = response.json().await?;

        if is_duplicate {
            warn!(asset_id = %body.id, "server reported duplicate asset");
        } else {
            debug!(asset_id = %body.id, "asset uploaded successfully");
        }

        Ok(UploadResult {
            asset_id: body.id,
            is_duplicate,
        })
    }

    /// Check a batch of assets against the server to find out which already
    /// exist before uploading them.
    ///
    /// This is the second deduplication layer (after the local SQLite check).
    /// Use the SHA-1 hex string for `checksum`.
    ///
    /// # Returns
    ///
    /// One `BulkCheckResult` per item in `items`, in the same order.
    #[instrument(skip(self, items), fields(count = items.len()))]
    pub async fn check_bulk_upload(
        &self,
        items: Vec<BulkCheckItem>,
    ) -> Result<Vec<BulkCheckResult>, ApiError> {
        debug!("checking {} assets for duplicates", items.len());

        #[derive(Serialize)]
        struct RequestBody {
            assets: Vec<BulkCheckItem>,
        }

        let response = self
            .client
            .post(self.url("/api/assets/bulk-upload-check"))
            .json(&RequestBody { assets: items })
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::map_status_error(response).await);
        }

        let body: BulkUploadCheckResponse = response.json().await?;
        debug!("{} results returned from bulk check", body.results.len());
        Ok(body.results)
    }
}

// ---------------------------------------------------------------------------
// AssetUploader trait implementation
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl crate::upload::worker::AssetUploader for ImmichClient {
    async fn upload_asset(
        &self,
        file_path: &Path,
        device_asset_id: &str,
        device_id: &str,
        created_at: DateTime<Utc>,
        modified_at: DateTime<Utc>,
    ) -> anyhow::Result<crate::upload::worker::UploadResult> {
        let result = self
            .upload_asset(
                file_path,
                device_asset_id,
                device_id,
                created_at,
                modified_at,
            )
            .await?;

        Ok(crate::upload::worker::UploadResult {
            asset_id: result.asset_id,
            is_duplicate: result.is_duplicate,
        })
    }

    async fn add_asset_to_album(&self, album_name: &str, asset_id: &str) -> anyhow::Result<()> {
        // Get or create album by name.
        let albums = self.get_albums().await?;
        let album_id = if let Some(album) = albums.iter().find(|a| a.album_name == album_name) {
            album.id.clone()
        } else {
            let album = self.create_album(album_name).await?;
            album.id
        };

        self.add_assets_to_album(&album_id, vec![asset_id.to_string()])
            .await?;
        Ok(())
    }

    async fn check_duplicates(
        &self,
        items: Vec<crate::upload::worker::BulkCheckItem>,
    ) -> anyhow::Result<Vec<crate::upload::worker::BulkCheckResult>> {
        let api_items: Vec<BulkCheckItem> = items
            .into_iter()
            .map(|i| BulkCheckItem {
                id: i.id,
                checksum: i.checksum,
            })
            .collect();

        let results = self.check_bulk_upload(api_items).await?;

        Ok(results
            .into_iter()
            .map(|r| {
                let exists = r.is_duplicate();
                crate::upload::worker::BulkCheckResult {
                    id: r.id,
                    exists,
                    asset_id: r.asset_id,
                }
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return a best-effort MIME type string for a file based on its extension.
///
/// Immich does not require an accurate value, but providing one improves
/// compatibility with future server-side processing.
fn mime_from_extension(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" | "heif" => "image/heic",
        "avif" => "image/avif",
        "tiff" | "tif" => "image/tiff",
        "bmp" => "image/bmp",
        // RAW formats — treated as octet-stream; Immich handles them server-side.
        "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" | "pef" | "srw" | "raf" => {
            "application/octet-stream"
        }
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "3gp" => "video/3gpp",
        "mts" | "m2ts" => "video/mp2t",
        _ => "application/octet-stream",
    }
}
