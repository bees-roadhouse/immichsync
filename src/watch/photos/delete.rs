// Delete from Photos after a verified upload (#116, ADR 0002 Amendment 1).
//
// Off by default. When armed (`[photos.delete]`, enabled plus both
// acknowledgements), Settings first asks for a `plan`, a dry run: every
// asset ImmichSync uploaded is checked against the local rules (age,
// favourites, albums, bursts, user library, every resource uploaded), and
// each survivor's resources are verified on the server again (present, not
// trashed, checksum equal to the SHA-1 we recorded). `execute` deletes
// exactly the previewed set, re-planned and re-verified, as one batch: one
// macOS confirmation. Every attempt is logged.
//
// The only write to the library is `DeleteLibrary::delete_assets`, called
// from `execute` alone; the guard test in `platform::photos` enforces that.

use chrono::{DateTime, Utc};

use crate::config::PhotosDeleteConfig;
use crate::upload::queue::QueueStore;

/// The most photos one batch (one macOS prompt) deletes.
pub const MAX_BATCH: usize = 500;

/// What PhotoKit says about an asset, for the delete rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetFacts {
    pub local_id: String,
    pub created_at: DateTime<Utc>,
    pub is_favorite: bool,
    pub in_user_album: bool,
    /// Part of a burst (the pick or a frame).
    pub is_burst: bool,
    /// `sourceType` includes the user library.
    pub user_library: bool,
    pub is_video: bool,
    /// A Live Photo (has a paired video).
    pub is_live: bool,
}

/// One uploaded resource, as recorded in `photos_assets`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub resource: String,
    pub sha1: String,
    pub immich_asset_id: String,
    /// Bytes, from `uploaded_files`, for the preview's total.
    pub size: u64,
}

/// The server's view of one asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerAsset {
    /// Immich's checksum: base64 of the SHA-1 of the original.
    pub checksum_b64: String,
    pub trashed: bool,
}

/// Why a photo is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    Burst,
    NotUserLibrary,
    Favorite,
    InAlbum,
    TooRecent,
    NotFullyUploaded,
    ServerMissing,
    ServerMismatch,
    ServerTrashed,
    ServerUnreachable,
    /// No longer in the library, or not previewed.
    NotOffered,
}

/// The resources a photo must have uploaded before it may be deleted.
pub fn required_resources(facts: &AssetFacts) -> &'static [&'static str] {
    if facts.is_video {
        &["video"]
    } else if facts.is_live {
        &["photo", "paired_video"]
    } else {
        &["photo"]
    }
}

/// Local rules (everything but the server check).
pub fn decide(
    facts: &AssetFacts,
    recorded: &[Recorded],
    cfg: &PhotosDeleteConfig,
    now: DateTime<Utc>,
) -> Result<(), Keep> {
    // Bursts: only the pick is uploaded, so the unit is never fully in Immich.
    if facts.is_burst {
        return Err(Keep::Burst);
    }
    if !facts.user_library {
        return Err(Keep::NotUserLibrary);
    }
    if cfg.keep_favorites && facts.is_favorite {
        return Err(Keep::Favorite);
    }
    if cfg.keep_in_albums && facts.in_user_album {
        return Err(Keep::InAlbum);
    }
    if now - facts.created_at < chrono::Duration::days(i64::from(cfg.min_age_days)) {
        return Err(Keep::TooRecent);
    }
    let complete = required_resources(facts).iter().all(|want| {
        recorded
            .iter()
            .any(|r| r.resource == *want && !r.sha1.is_empty() && !r.immich_asset_id.is_empty())
    });
    if !complete {
        return Err(Keep::NotFullyUploaded);
    }
    Ok(())
}

/// Does the server's base64 checksum equal our hex SHA-1?
pub fn checksum_matches(server_b64: &str, sha1_hex: &str) -> bool {
    use base64::Engine as _;
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(server_b64.trim()) else {
        return false;
    };
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    !hex.is_empty() && hex.eq_ignore_ascii_case(sha1_hex.trim())
}

/// Verify every required resource of an asset on the server.
async fn verify_on_server(
    facts: &AssetFacts,
    recorded: &[Recorded],
    server: &dyn ServerAssets,
) -> Result<(), Keep> {
    for want in required_resources(facts) {
        let Some(r) = recorded.iter().find(|r| r.resource == *want) else {
            return Err(Keep::NotFullyUploaded);
        };
        match server.asset(&r.immich_asset_id).await {
            Err(_) => return Err(Keep::ServerUnreachable),
            Ok(None) => return Err(Keep::ServerMissing),
            Ok(Some(a)) if a.trashed => return Err(Keep::ServerTrashed),
            Ok(Some(a)) if !checksum_matches(&a.checksum_b64, &r.sha1) => {
                return Err(Keep::ServerMismatch)
            }
            Ok(Some(_)) => {}
        }
    }
    Ok(())
}

/// Every asset still eligible after the local rules and the server check,
/// with its verified Immich ids; `only` narrows it to a previewed set.
async fn eligible(
    cfg: &PhotosDeleteConfig,
    store: &dyn QueueStore,
    library: &dyn DeleteLibrary,
    server: &dyn ServerAssets,
    now: DateTime<Utc>,
    only: Option<&[String]>,
) -> anyhow::Result<(Vec<(String, Vec<String>, u64)>, Vec<(String, Keep)>)> {
    use std::collections::BTreeMap;
    let mut by_asset: BTreeMap<String, Vec<Recorded>> = BTreeMap::new();
    for (local_id, r) in store.photos_uploaded_resources()? {
        if only.is_none_or(|ids| ids.contains(&local_id)) {
            by_asset.entry(local_id).or_default().push(r);
        }
    }
    let ids: Vec<String> = by_asset.keys().cloned().collect();
    let facts = library.facts(&ids);

    let mut yes = Vec::new();
    let mut kept = Vec::new();
    for id in &ids {
        let Some(f) = facts.iter().find(|f| &f.local_id == id) else {
            kept.push((id.clone(), Keep::NotOffered));
            continue;
        };
        let recorded = &by_asset[id];
        let verdict = match decide(f, recorded, cfg, now) {
            Ok(()) => verify_on_server(f, recorded, server).await,
            Err(k) => Err(k),
        };
        match verdict {
            Ok(()) => {
                let wanted = required_resources(f);
                let used: Vec<&Recorded> = recorded
                    .iter()
                    .filter(|r| wanted.contains(&r.resource.as_str()))
                    .collect();
                yes.push((
                    id.clone(),
                    used.iter().map(|r| r.immich_asset_id.clone()).collect(),
                    used.iter().map(|r| r.size).sum(),
                ));
            }
            Err(k) => kept.push((id.clone(), k)),
        }
    }
    Ok((yes, kept))
}

/// The library side: facts for the rules, and the one write path.
pub trait DeleteLibrary {
    /// Facts for the assets still in the library (missing ones are left out).
    fn facts(&self, local_ids: &[String]) -> Vec<AssetFacts>;
    /// Move these assets to Recently Deleted. macOS asks the user first;
    /// `Ok(false)` means they declined.
    fn delete_assets(&self, local_ids: &[String]) -> anyhow::Result<bool>;
}

/// The server side: one asset's state, `None` if it doesn't exist.
// async-trait's generated signatures trip clippy's double_must_use, as on
// `AssetUploader`; the code is the macro's.
#[allow(clippy::double_must_use)]
#[async_trait::async_trait]
pub trait ServerAssets: Send + Sync {
    async fn asset(&self, immich_asset_id: &str) -> anyhow::Result<Option<ServerAsset>>;
}

/// A dry run's result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeletePlan {
    /// Assets that would be deleted.
    pub delete: Vec<String>,
    /// Their uploaded size.
    pub bytes: u64,
    /// Everything else considered, with the reason it's kept.
    pub kept: Vec<(String, Keep)>,
}

/// What `execute` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// Not armed: nothing considered, nothing deleted.
    Off,
    /// Nothing left to delete after re-verification.
    Nothing,
    Deleted(usize),
    /// The user said no at the macOS prompt.
    Declined(usize),
}

/// Dry run. Empty when not armed.
pub async fn plan(
    cfg: &PhotosDeleteConfig,
    store: &dyn QueueStore,
    library: &dyn DeleteLibrary,
    server: &dyn ServerAssets,
    now: DateTime<Utc>,
) -> anyhow::Result<DeletePlan> {
    if !cfg.armed() {
        return Ok(DeletePlan::default());
    }
    let (yes, kept) = eligible(cfg, store, library, server, now, None).await?;
    Ok(DeletePlan {
        bytes: yes.iter().map(|(_, _, b)| b).sum(),
        delete: yes.into_iter().map(|(id, _, _)| id).collect(),
        kept,
    })
}

/// Delete the previewed assets that still qualify, as one batch.
pub async fn execute(
    previewed: &[String],
    cfg: &PhotosDeleteConfig,
    store: &dyn QueueStore,
    library: &dyn DeleteLibrary,
    server: &dyn ServerAssets,
    now: DateTime<Utc>,
) -> anyhow::Result<DeleteOutcome> {
    if !cfg.armed() {
        return Ok(DeleteOutcome::Off);
    }
    // Exactly the previewed set, re-checked now: anything whose server copy
    // changed since the preview drops out.
    let (mut yes, _) = eligible(cfg, store, library, server, now, Some(previewed)).await?;
    if yes.is_empty() {
        return Ok(DeleteOutcome::Nothing);
    }
    yes.truncate(MAX_BATCH);
    let ids: Vec<String> = yes.iter().map(|(id, _, _)| id.clone()).collect();

    let result = library.delete_assets(&ids);
    let outcome = match &result {
        Ok(true) => "deleted".to_string(),
        Ok(false) => "declined".to_string(),
        Err(e) => format!("failed: {e:#}"),
    };
    for (id, immich, _) in &yes {
        store.record_photos_deletion(id, &immich.join(","), &outcome)?;
    }
    match result {
        Ok(true) => {
            tracing::info!(
                count = ids.len(),
                "Deleted photos from the library (to Recently Deleted)"
            );
            Ok(DeleteOutcome::Deleted(ids.len()))
        }
        Ok(false) => {
            tracing::info!(
                count = ids.len(),
                "Photo deletion declined at the macOS prompt"
            );
            Ok(DeleteOutcome::Declined(ids.len()))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbStore};
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn armed() -> PhotosDeleteConfig {
        PhotosDeleteConfig {
            enabled: true,
            ack_icloud: true,
            ack_no_shared_library: true,
            ..PhotosDeleteConfig::default()
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-10-19T12:00:00Z".parse().unwrap()
    }

    fn old_photo(id: &str) -> AssetFacts {
        AssetFacts {
            local_id: id.to_string(),
            created_at: "2026-01-01T00:00:00Z".parse().unwrap(),
            is_favorite: false,
            in_user_album: false,
            is_burst: false,
            user_library: true,
            is_video: false,
            is_live: false,
        }
    }

    fn rec(resource: &str, sha: &str, id: &str) -> Recorded {
        Recorded {
            resource: resource.into(),
            sha1: sha.into(),
            immich_asset_id: id.into(),
            size: 1000,
        }
    }

    // ── local rules ─────────────────────────────────────────────────────

    #[test]
    fn the_rules_keep_what_the_safeguards_say() {
        let cfg = armed();
        let photo = [rec("photo", "aa", "imm-1")];
        assert_eq!(decide(&old_photo("A"), &photo, &cfg, now()), Ok(()));

        let recent = AssetFacts {
            created_at: "2026-10-01T00:00:00Z".parse().unwrap(),
            ..old_photo("A")
        };
        assert_eq!(decide(&recent, &photo, &cfg, now()), Err(Keep::TooRecent));

        let fav = AssetFacts {
            is_favorite: true,
            ..old_photo("A")
        };
        assert_eq!(decide(&fav, &photo, &cfg, now()), Err(Keep::Favorite));
        let mut no_fav_rule = cfg.clone();
        no_fav_rule.keep_favorites = false;
        assert_eq!(decide(&fav, &photo, &no_fav_rule, now()), Ok(()));

        let in_album = AssetFacts {
            in_user_album: true,
            ..old_photo("A")
        };
        assert_eq!(
            decide(&in_album, &photo, &cfg, now()),
            Ok(()),
            "albums kept only if asked"
        );
        let mut keep_albums = cfg.clone();
        keep_albums.keep_in_albums = true;
        assert_eq!(
            decide(&in_album, &photo, &keep_albums, now()),
            Err(Keep::InAlbum)
        );

        let burst = AssetFacts {
            is_burst: true,
            ..old_photo("A")
        };
        assert_eq!(decide(&burst, &photo, &cfg, now()), Err(Keep::Burst));

        let shared = AssetFacts {
            user_library: false,
            ..old_photo("A")
        };
        assert_eq!(
            decide(&shared, &photo, &cfg, now()),
            Err(Keep::NotUserLibrary)
        );

        assert_eq!(
            decide(&old_photo("A"), &[], &cfg, now()),
            Err(Keep::NotFullyUploaded)
        );
        // A dedup-skipped resource with no Immich id isn't proof.
        assert_eq!(
            decide(&old_photo("A"), &[rec("photo", "aa", "")], &cfg, now()),
            Err(Keep::NotFullyUploaded)
        );
    }

    #[test]
    fn a_live_photo_needs_both_resources_and_a_video_needs_its_video() {
        let cfg = armed();
        let live = AssetFacts {
            is_live: true,
            ..old_photo("L")
        };
        assert_eq!(required_resources(&live), &["photo", "paired_video"]);
        assert_eq!(
            decide(&live, &[rec("photo", "aa", "imm-1")], &cfg, now()),
            Err(Keep::NotFullyUploaded),
            "the still alone isn't the unit"
        );
        assert_eq!(
            decide(
                &live,
                &[
                    rec("photo", "aa", "imm-1"),
                    rec("paired_video", "bb", "imm-2")
                ],
                &cfg,
                now()
            ),
            Ok(())
        );
        let video = AssetFacts {
            is_video: true,
            ..old_photo("V")
        };
        assert_eq!(required_resources(&video), &["video"]);
        assert_eq!(
            decide(&video, &[rec("photo", "aa", "x")], &cfg, now()),
            Err(Keep::NotFullyUploaded)
        );
    }

    #[test]
    fn checksums_compare_base64_to_hex() {
        // SHA-1 of "abc".
        let hex = "a9993e364706816aba3e25717850c26c9cd0d89d";
        let b64 = "qZk+NkcGgWq6PiVxeFDCbJzQ2J0=";
        assert!(checksum_matches(b64, hex));
        assert!(checksum_matches(b64, &hex.to_uppercase()));
        assert!(!checksum_matches(
            b64,
            "a9993e364706816aba3e25717850c26c9cd0d89e"
        ));
        assert!(!checksum_matches("not base64!", hex));
    }

    // ── plan / execute ──────────────────────────────────────────────────

    #[derive(Default)]
    struct FakeLibrary {
        facts: HashMap<String, AssetFacts>,
        deleted: Mutex<Vec<Vec<String>>>,
        decline: bool,
    }

    impl DeleteLibrary for FakeLibrary {
        fn facts(&self, local_ids: &[String]) -> Vec<AssetFacts> {
            local_ids
                .iter()
                .filter_map(|id| self.facts.get(id).cloned())
                .collect()
        }
        fn delete_assets(&self, local_ids: &[String]) -> anyhow::Result<bool> {
            self.deleted.lock().unwrap().push(local_ids.to_vec());
            Ok(!self.decline)
        }
    }

    #[derive(Default)]
    struct FakeServer {
        assets: Mutex<HashMap<String, ServerAsset>>,
        down: bool,
    }

    #[async_trait::async_trait]
    impl ServerAssets for FakeServer {
        async fn asset(&self, id: &str) -> anyhow::Result<Option<ServerAsset>> {
            if self.down {
                anyhow::bail!("unreachable");
            }
            Ok(self.assets.lock().unwrap().get(id).cloned())
        }
    }

    /// SHA-1 hex → Immich's base64.
    fn b64(hex: &str) -> String {
        use base64::Engine as _;
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    const SHA_A: &str = "a9993e364706816aba3e25717850c26c9cd0d89d";
    const SHA_B: &str = "84983e441c3bd26ebaae4aa1f95129e5e54670f1";
    const SHA_C: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";

    /// A store with three uploaded photos (A, B, C), a library that has
    /// them, and a server that holds them with matching checksums.
    fn world() -> (DbStore, FakeLibrary, FakeServer) {
        let store = DbStore::new(Database::open_in_memory().unwrap());
        let server = FakeServer::default();
        let mut lib = FakeLibrary::default();
        for (id, sha) in [("A", SHA_A), ("B", SHA_B), ("C", SHA_C)] {
            let imm = format!("imm-{id}");
            {
                let db = store.inner().lock().unwrap();
                db.record_upload(
                    &format!("photos:{id}/photo"),
                    sha,
                    2048,
                    0,
                    Some(&imm),
                    "d",
                    "",
                )
                .unwrap();
                db.record_photos_asset(id, "photo", sha, &imm, None)
                    .unwrap();
            }
            lib.facts.insert(id.into(), old_photo(id));
            server.assets.lock().unwrap().insert(
                imm,
                ServerAsset {
                    checksum_b64: b64(sha),
                    trashed: false,
                },
            );
        }
        (store, lib, server)
    }

    #[tokio::test]
    async fn off_means_nothing_is_considered_or_deleted() {
        let (store, lib, server) = world();
        let off = PhotosDeleteConfig::default();
        assert_eq!(
            plan(&off, &store, &lib, &server, now()).await.unwrap(),
            DeletePlan::default()
        );
        let ids = vec!["A".to_string(), "B".to_string()];
        assert_eq!(
            execute(&ids, &off, &store, &lib, &server, now())
                .await
                .unwrap(),
            DeleteOutcome::Off
        );
        assert!(lib.deleted.lock().unwrap().is_empty(), "no write path ran");
    }

    #[tokio::test]
    async fn the_plan_offers_only_server_verified_photos() {
        let (store, mut lib, server) = world();
        // B: the server copy's checksum differs. C: in Immich's trash.
        server
            .assets
            .lock()
            .unwrap()
            .get_mut("imm-B")
            .unwrap()
            .checksum_b64 = b64(SHA_C);
        server
            .assets
            .lock()
            .unwrap()
            .get_mut("imm-C")
            .unwrap()
            .trashed = true;
        // D: uploaded, but gone from the server.
        {
            let db = store.inner().lock().unwrap();
            db.record_photos_asset("D", "photo", SHA_A, "imm-D", None)
                .unwrap();
        }
        lib.facts.insert("D".into(), old_photo("D"));

        let p = plan(&armed(), &store, &lib, &server, now()).await.unwrap();
        assert_eq!(p.delete, vec!["A".to_string()]);
        assert_eq!(p.bytes, 2048);
        let kept: HashMap<_, _> = p.kept.into_iter().collect();
        assert_eq!(kept["B"], Keep::ServerMismatch);
        assert_eq!(kept["C"], Keep::ServerTrashed);
        assert_eq!(kept["D"], Keep::ServerMissing);
    }

    #[tokio::test]
    async fn an_unreachable_server_keeps_everything() {
        let (store, lib, mut server) = world();
        server.down = true;
        let p = plan(&armed(), &store, &lib, &server, now()).await.unwrap();
        assert!(p.delete.is_empty());
        assert!(p.kept.iter().all(|(_, k)| *k == Keep::ServerUnreachable));
    }

    #[tokio::test]
    async fn execute_deletes_only_previewed_photos_that_still_verify_in_one_batch() {
        let (store, lib, server) = world();
        let preview = plan(&armed(), &store, &lib, &server, now()).await.unwrap();
        assert_eq!(preview.delete.len(), 3);

        // Between preview and confirm, B's server copy went missing.
        server.assets.lock().unwrap().remove("imm-B");
        let previewed: Vec<String> = vec!["A".into(), "B".into()]; // C not previewed

        let out = execute(&previewed, &armed(), &store, &lib, &server, now())
            .await
            .unwrap();
        assert_eq!(out, DeleteOutcome::Deleted(1));
        assert_eq!(
            *lib.deleted.lock().unwrap(),
            vec![vec!["A".to_string()]],
            "one batch, A only"
        );

        let log = store.inner().lock().unwrap().photos_deletions(10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].local_id, "A");
        assert_eq!(log[0].outcome, "deleted");
        assert_eq!(log[0].immich_asset_ids, "imm-A");

        // A deleted photo is never offered again.
        let again = plan(&armed(), &store, &lib, &server, now()).await.unwrap();
        assert!(!again.delete.contains(&"A".to_string()));
    }

    #[tokio::test]
    async fn a_declined_prompt_is_logged_and_offered_again_later() {
        let (store, mut lib, server) = world();
        lib.decline = true;
        let ids = vec!["A".to_string()];
        let out = execute(&ids, &armed(), &store, &lib, &server, now())
            .await
            .unwrap();
        assert_eq!(out, DeleteOutcome::Declined(1));
        let log = store.inner().lock().unwrap().photos_deletions(10).unwrap();
        assert_eq!(log[0].outcome, "declined");
        let again = plan(&armed(), &store, &lib, &server, now()).await.unwrap();
        assert!(again.delete.contains(&"A".to_string()));
    }

    #[tokio::test]
    async fn a_batch_is_capped() {
        let (store, mut lib, server) = world();
        {
            let db = store.inner().lock().unwrap();
            for i in 0..(MAX_BATCH + 5) {
                let id = format!("P{i}");
                db.record_photos_asset(&id, "photo", SHA_A, "imm-A", None)
                    .unwrap();
            }
        }
        for i in 0..(MAX_BATCH + 5) {
            let id = format!("P{i}");
            lib.facts.insert(id.clone(), old_photo(&id));
        }
        let preview = plan(&armed(), &store, &lib, &server, now()).await.unwrap();
        let out = execute(&preview.delete, &armed(), &store, &lib, &server, now())
            .await
            .unwrap();
        assert_eq!(out, DeleteOutcome::Deleted(MAX_BATCH));
        assert_eq!(lib.deleted.lock().unwrap()[0].len(), MAX_BATCH);
    }
}
