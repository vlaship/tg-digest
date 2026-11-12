//! Photos for the digest.
//!
//! For every topic we take the photos of one source post (see `digest::pick_photos`).
//! The personal account downloads them once, when the parts are written to the journal:
//! the bot cannot read a channel where it is not an admin. Each file is written in the
//! `photos` folder next to the database (in a container: `/data/photos`) before the next
//! download starts, so a resend after a restart posts exactly the same and the process
//! does not keep a whole album in memory. When the run is finished, its folder is deleted.

use anyhow::{Context, Result};
use grammers_client::client::Client;
use std::path::{Path, PathBuf};

use crate::db::Db;
use crate::telegram::client::peer_ref;

/// The Bot API limit for a photo. A bigger file is skipped.
const MAX_PHOTO_BYTES: usize = 10 * 1024 * 1024;

/// Photos of one part that were written to the folder.
pub struct SavedPhotos {
    pub count: usize,
    pub bytes: usize,
}

impl SavedPhotos {
    pub fn none() -> Self {
        Self { count: 0, bytes: 0 }
    }
}

/// The photos folder: next to the database file.
pub fn dir(db_file: &str) -> PathBuf {
    Path::new(db_file)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("photos")
}

fn file(dir: &Path, run_id: i64, idx: usize, pos: usize) -> PathBuf {
    dir.join(run_id.to_string())
        .join(format!("{idx}-{pos}.jpg"))
}

/// Download the photos of these messages and write each one before the next
/// download. A message without a photo, or a photo over 10 MB, is skipped.
/// Reading stops as soon as the file passes the limit, so the oversized bytes
/// are not kept.
pub async fn download(
    client: &Client,
    db: &Db,
    channel_id: i64,
    ids: &[i32],
    dir: &Path,
    run_id: i64,
    idx: usize,
) -> Result<SavedPhotos> {
    let channel = db
        .channel(channel_id)
        .await?
        .with_context(|| format!("channel id {channel_id} is not in the database"))?;
    let peer = peer_ref(channel.id, channel.access_hash)?;
    let messages = client
        .get_messages_by_id(peer, ids)
        .await
        .with_context(|| format!("could not read the posts of {}", channel.label()))?;

    let mut saved = SavedPhotos::none();
    let mut folder_ready = false;
    for message in messages.into_iter().flatten() {
        let Some(photo) = message.photo() else {
            continue;
        };
        // The size is known before the download. Skip it without reading.
        if photo.size().is_some_and(|size| size > MAX_PHOTO_BYTES) {
            tracing::warn!(
                channel = %channel.label(),
                msg_id = message.id(),
                bytes = photo.size().unwrap_or(0),
                "photo skipped: too big"
            );
            continue;
        }

        let mut data = Vec::new();
        let mut too_big = false;
        let mut stream = client.iter_download(&photo);
        let download_err = loop {
            match stream.next().await {
                Ok(Some(chunk)) => {
                    if data.len().saturating_add(chunk.len()) > MAX_PHOTO_BYTES {
                        too_big = true;
                        break None;
                    }
                    data.extend_from_slice(&chunk);
                }
                Ok(None) => break None,
                Err(e) => break Some(e),
            }
        };
        drop(stream);
        if let Some(e) = download_err {
            // Earlier photos of this part are already on disk. Drop them so a
            // retry does not send a short album.
            remove_part(dir, run_id, idx, saved.count);
            return Err(e).context("could not download a photo");
        }

        if too_big || data.is_empty() {
            tracing::warn!(
                channel = %channel.label(),
                msg_id = message.id(),
                bytes = data.len(),
                "photo skipped: empty or too big"
            );
            continue;
        }

        if !folder_ready {
            let folder = dir.join(run_id.to_string());
            if let Err(e) = std::fs::create_dir_all(&folder) {
                remove_part(dir, run_id, idx, saved.count);
                return Err(e).with_context(|| format!("could not create {}", folder.display()));
            }
            folder_ready = true;
        }
        let path = file(dir, run_id, idx, saved.count);
        if let Err(e) = std::fs::write(&path, &data) {
            // The write may have left a short file. Drop it with the rest of this part.
            remove_part(dir, run_id, idx, saved.count + 1);
            return Err(e).with_context(|| format!("could not write {}", path.display()));
        }
        saved.bytes += data.len();
        saved.count += 1;
        // The bytes are on disk. Drop them before the next download.
        drop(data);
    }
    Ok(saved)
}

/// True when every photo file of this part is on disk. Used before send so a
/// missing folder falls back to text without loading the album into memory.
pub fn files_ready(dir: &Path, run_id: i64, idx: usize, count: usize) -> Result<()> {
    for pos in 0..count {
        let path = file(dir, run_id, idx, pos);
        if !path.is_file() {
            anyhow::bail!("photo file missing: {}", path.display());
        }
    }
    Ok(())
}

/// Read one photo of a part. Call once per upload so the album is not held in RAM.
pub fn read(dir: &Path, run_id: i64, idx: usize, pos: usize) -> Result<Vec<u8>> {
    let path = file(dir, run_id, idx, pos);
    std::fs::read(&path).with_context(|| format!("could not read {}", path.display()))
}

/// The run is finished: its photos are not needed any more.
pub fn remove(dir: &Path, run_id: i64) {
    let folder = dir.join(run_id.to_string());
    if let Err(e) = std::fs::remove_dir_all(&folder) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(error = %e, folder = %folder.display(), "could not delete the photos");
        }
    }
}

/// A download failed after some files of this part were written. Drop them so a
/// retry does not send a short album.
fn remove_part(dir: &Path, run_id: i64, idx: usize, count: usize) {
    for pos in 0..count {
        let path = file(dir, run_id, idx, pos);
        if let Err(e) = std::fs::remove_file(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(error = %e, file = %path.display(), "could not delete a photo");
            }
        }
    }
}
