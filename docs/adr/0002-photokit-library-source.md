# 0002 — Apple Photos library as a second watch source (PhotoKit)

- **Status:** proposed (2026-10-18)
- **Decided upstream:** Nate, 2026-10-18, via Pia: "both", meaning folder sync *and* the Photos library. Folder sync came first (#59–#64). This note shapes the library half before any code is written (#66).
- **Builds on:** [0001](0001-macos-platform-seam.md) (the platform seam and the `.app` bundle, #63).

## Context

The folder watcher sees files. The Photos library is not a folder of files. `Photos Library.photoslibrary` is a private store whose layout Apple changes between releases. With *Optimize Mac Storage* on, most originals exist only in iCloud. Edits, Live Photo pairs and bursts are database relationships, not files. Since 0001 the filter skips anything inside a `.photoslibrary` for exactly this reason.

The supported way in is **PhotoKit**: `PHPhotoLibrary`, `PHAsset`, `PHAssetResource` and `PHAssetResourceManager`. It needs a bundle with a usage string and asks the user for permission (TCC). It hands us asset records, not paths.

## Decision

### Shape: a source that exports to temp files and joins the existing pipeline

```
PhotoKit ──(enumerate / change token)──► PhotosSource ──(export original to temp file)──►
  UploadQueue::process_file (hash → dedup → upload → album) ──► delete temp file
```

- New module `src/watch/photos/` (macOS only, behind `platform::photos` for the Objective-C calls via `objc2-photos`). It runs on its own thread, because PhotoKit's calls block and use completion blocks.
- An asset becomes a queue entry with `source = 'photos'` and its `localIdentifier`, **not a path**. The temp export is made when a worker picks the entry up and deleted when the entry finishes or fails, so a retry re-exports instead of relying on a temp file still being there. Schema v4: `upload_queue.source`, `upload_queue.photos_local_id`, a `photos_assets` table (`local_id`, `resource`, `sha1`, `immich_asset_id`, `modified_at`), and the persistent change token in `config`.
- Everything after export is the existing pipeline: SHA-1, the four dedup layers, upload, album, retry and backoff. The pipeline does not learn about PhotoKit.

### Read-only access and the permission prompt

- PhotoKit has no read-only level: `.addOnly` cannot read, so we request `.readWrite` and **never call a change request**. The code has no `PHAssetChangeRequest` anywhere; a grep enforces this in review. Info.plist gets `NSPhotoLibraryUsageDescription`: "ImmichSync reads your Photos library to back it up to your Immich server. It never changes or deletes photos." A Developer-ID build with hardened runtime also needs the `com.apple.security.personal-information.photos-library` entitlement.
- **Opt-in, and the prompt only follows the user's own action.** The library is off by default. Settings gets a *Photos library* section with **Sync my Photos library**. Ticking it calls `requestAuthorization(for: .readWrite)`, so the system prompt appears as the direct result of that click, never at startup or first run. The section shows the status:
  - *Not determined*: the toggle.
  - *Authorized*: the asset count and the last sync time.
  - *Limited* (the user picked a subset): "ImmichSync can see N selected items", plus a button to change the selection.
  - *Denied / Restricted*: an explanation and **Open Privacy Settings** (`x-apple.systempreferences:com.apple.preference.security?Privacy_Photos`). We never re-prompt; macOS wouldn't show it again anyway.
- Without a bundle (a bare dev binary) the section says the library needs the `.app`, and does nothing.

### What gets enumerated

- `PHAsset` images and videos, user library only. **Excluded by default:** hidden assets (`includeHiddenAssets = false`), shared-album and iCloud Shared Library items that aren't the user's own (`includeAssetSourceTypes = .typeUserLibrary`), and non-representative burst frames (`includeAllBurstAssets = false`). Recently Deleted is never returned.
- Order: newest first, matching the folder default (`upload.order`).

### Incremental sync: persistent change tokens

- `PHPhotoLibrary.fetchPersistentChanges(since:)` (macOS 13+) returns inserts and updates since a stored `PHPersistentChangeToken`, and it survives restarts and reboots. That is the library's equivalent of FSEvents' history. While the app runs, a `PHPhotoLibraryChangeObserver` wakes the source, which then reads persistent changes. A single code path handles both.
- The first run, or a `persistentChangeTokenExpired` error, triggers a full enumeration; `photos_assets` keeps it from re-uploading.
- **Updates, meaning edits:** we upload originals, so an edit doesn't change our bytes and is ignored. **Deletes:** never propagated. ImmichSync uploads; it doesn't mirror deletions, the same as for folders.

### Originals, edits, HEIC

- Export the **original** resource (`PHAssetResource` of type `.photo` / `.video`), never the rendered edit (`.fullSizePhoto`). That is what Immich wants for dedup and what the iPhone Immich app uploads. HEIC, HEVC and ProRAW go up **as-is**; Immich decodes them, and we never transcode.
- The original filename comes from `PHAssetResource.originalFilename`. Dates come from `PHAsset.creationDate` / `modificationDate`, passed as `fileCreatedAt` / `fileModifiedAt` (EXIF inside the original takes precedence on the server anyway).
- `deviceAssetId` = `photos-<localIdentifier>-<resource>`, stable across exports.

### Live Photos

- A Live Photo is two resources, `.photo` (HEIC/JPEG) and `.pairedVideo` (MOV), sharing an Apple ContentIdentifier in their metadata. Export and upload **both**: the video first, then the still with `livePhotoVideoId` set to the video's Immich id. Immich also links pairs server-side by ContentIdentifier, so the explicit id is belt and braces. The Live Photo slice verifies on pics.trh which of the two the server version honours.
- In `photos_assets` a pair is two rows under one `local_id`. The still is not marked done until its video is.

### iCloud-optimised originals: download, politely

- With *Optimize Mac Storage* on, most originals are iCloud-only, so skipping them would back up a fraction of the library. **Default: download** (`PHAssetResourceRequestOptions.isNetworkAccessAllowed = true`), **one at a time**, through the existing bandwidth limit. Each temp export is deleted as soon as its upload settles. A disk-space guard pauses exports while free space on the data volume is below a floor (2 GiB, or the asset size × 2).
- `photos.icloud_originals = "download" | "skip"`. With `skip`, iCloud-only assets are recorded as *deferred* and counted in Settings ("1,204 originals are only in iCloud"), never failed. They are retried if they later become local.

### Dedupe against folder sync

- Layer 2 (SHA-1 of the bytes) already spans sources: a photo exported both to `~/Pictures` and present in the library hashes the same, so whichever arrives second is skipped locally without an upload. Layer 3 (the server bulk check) catches what the iPhone Immich app already uploaded, because Immich's checksum is SHA-1 of the original.
- The expensive part is unavoidable: to learn an asset's SHA-1 we must export it, which downloads it from iCloud when it's optimised. The first pass over a large library costs bandwidth on the order of the library size. This is documented in Settings next to the toggle, before the user ticks it.

## Consequences

- One new source, with the pipeline unchanged. The macOS-only code lives in `src/watch/photos/` and `platform::photos`; Windows isn't affected.
- New crate: `objc2-photos` (same objc2 0.6 family).
- Schema v4 migration (the `upload_queue` source columns, `photos_assets`, the change token).
- The bundle (#63) gains `NSPhotoLibraryUsageDescription` and, for Developer ID builds, the photos-library entitlement.
- Testing: PhotoKit needs a real library and a TCC grant, so CI covers only the pure parts (`deviceAssetId`, pairing state, queue transitions, the disk guard). The tracer is verified by hand on Nate's Mac against the "immichsync test" album, like folder sync.

## Slices

1. **Tracer:** Settings toggle → permission prompt → enumerate → export and upload the **3 newest** originals (a temporary cap) into a fixed album. Schema v4. Local originals only; iCloud-only ones are logged and skipped.
2. **Full and incremental sync:** remove the cap; persistent change token plus change observer; `photos_assets`.
3. **iCloud originals:** the download policy, bandwidth, the disk guard, the *deferred* state and its count.
4. **Live Photos:** pair export and upload with `livePhotoVideoId`; verify linking on pics.trh.
5. **Settings polish:** status lines, Limited-library selection, Open Privacy Settings, tray counts.

## Open questions for Nate

1. iCloud originals: **download by default** (recommended), or skip?
2. Originals only (recommended), or also upload the edited version as a separate asset?
3. Hidden photos and shared-library items: excluded by default (recommended). Any reason to include them?
4. Map Photos albums to Immich albums? Not in these slices; it's a natural sixth.
