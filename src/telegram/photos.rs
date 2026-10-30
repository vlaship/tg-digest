//! Photos for the digest.
//!
//! For every topic we take the photos of one source post (see `digest::pick_photos`).
//! The personal account downloads them once, when the parts are written to the journal:
//! the bot cannot read a channel where it is not an admin. The files wait in the
//! `photos` folder next to the database (in a container: `/data/photos`), so a resend
//! after a restart posts exactly the same. When the run is finished, its folder is deleted.

use anyhow::{Context, Result};
use grammers_client::client::Client;
use std::path::{Path, PathBuf};

use crate::db::Db;
use crate::telegram::client::peer_ref;

/// The Bot API limit for a photo. A bigger file is skipped.
const MAX_PHOTO_BYTES: usize = 10 * 1024 * 1024;

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

/// Download the photos of these messages of the channel. A message without a photo
/// or a photo that is too big is skipped.
pub async fn download(
    client: &Client,
    db: &Db,
    channel_id: i64,
    ids: &[i32],
) -> Result<Vec<Vec<u8>>> {
    let channel = db
        .channel(channel_id)
        .await?
        .with_context(|| format!("channel id {channel_id} is not in the database"))?;
    let peer = peer_ref(channel.id, channel.access_hash)?;
    let messages = client
        .get_messages_by_id(peer, ids)
        .await
        .with_context(|| format!("could not read the posts of {}", channel.label()))?;

    let mut out = Vec::new();
    for message in messages.into_iter().flatten() {
        let Some(photo) = message.photo() else {
            continue;
        };
        let mut data = Vec::new();
        let mut download = client.iter_download(&photo);
        while let Some(chunk) = download
            .next()
            .await
            .context("could not download a photo")?
        {
            data.extend(chunk);
        }
        if data.is_empty() || data.len() > MAX_PHOTO_BYTES {
            tracing::warn!(
                channel = %channel.label(),
                msg_id = message.id(),
                bytes = data.len(),
                "photo skipped: empty or too big"
            );
            continue;
        }
        out.push(data);
    }
    Ok(out)
}

/// Write the photos of one part of the journal.
pub fn save(dir: &Path, run_id: i64, idx: usize, photos: &[Vec<u8>]) -> Result<()> {
    let folder = dir.join(run_id.to_string());
    std::fs::create_dir_all(&folder)
        .with_context(|| format!("could not create {}", folder.display()))?;
    for (pos, data) in photos.iter().enumerate() {
        let path = file(dir, run_id, idx, pos);
        std::fs::write(&path, data)
            .with_context(|| format!("could not write {}", path.display()))?;
    }
    Ok(())
}

/// Read the photos of one part back, in album order.
pub fn load(dir: &Path, run_id: i64, idx: usize, count: usize) -> Result<Vec<Vec<u8>>> {
    (0..count)
        .map(|pos| {
            let path = file(dir, run_id, idx, pos);
            std::fs::read(&path).with_context(|| format!("could not read {}", path.display()))
        })
        .collect()
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
