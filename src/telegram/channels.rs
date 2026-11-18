//! Channel lookups and name refresh.
//!
//! The channel list itself lives only in the database and is managed with `/add`
//! and `/rm` sent to the bot. Here we find channels in Telegram and read their
//! titles and usernames again before every run.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use grammers_client::client::Client;
use grammers_client::peer::Peer;
use grammers_client::tl;
use grammers_session::types::PeerKind;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use crate::db::{Channel, Db};
use crate::telegram::client::peer_ref;

/// A Telegram channel with its `access_hash`: with it we can read the channel without a
/// new resolve — this is why private channels work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelPeer {
    pub id: i64,
    pub access_hash: i64,
}

impl From<&Channel> for ChannelPeer {
    fn from(channel: &Channel) -> Self {
        Self {
            id: channel.id,
            access_hash: channel.access_hash,
        }
    }
}

/// A private channel by numeric id: we do not have its `access_hash`, and without it
/// `channels.getChannels` does not answer. We take the peer from the dialog list — it
/// is there if the account follows the channel.
pub async fn find_in_dialogs(client: &Client, id: i64) -> Result<Option<ChannelPeer>> {
    let mut dialogs = client.iter_dialogs();
    while let Some(dialog) = dialogs
        .next()
        .await
        .context("could not read the dialog list")?
    {
        let reference = dialog.peer_ref();
        // We must check the peer type: "bare" ids of a user, a group and a
        // channel live in different spaces and can easily be the same, and
        // we can read only channels.
        if reference.id.kind() == PeerKind::Channel && reference.id.bare_id() == Some(id) {
            return Ok(Some(ChannelPeer {
                id,
                access_hash: reference.auth.hash(),
            }));
        }
    }
    Ok(None)
}

/// What changed for one channel after a name refresh.
pub struct UpdatedChannel {
    pub id: i64,
    pub old_title: String,
    pub new_title: String,
    pub old_username: Option<String>,
    pub new_username: Option<String>,
}

/// Result of refreshing titles and usernames for a list of channels.
pub struct RefreshSummary {
    pub checked: usize,
    pub updated: Vec<UpdatedChannel>,
    pub unavailable: Vec<i64>,
}

/// Update titles and usernames of the enabled channels without changing the list.
///
/// Called before every run: otherwise a channel that was renamed or made private
/// would stay with a stale link in the digest forever. Returns the number of
/// channels where something changed.
pub async fn refresh_names(client: &Client, db: &Db) -> Result<usize> {
    let channels = db.enabled_channels().await?;
    let summary = refresh_list(client, db, channels).await?;
    Ok(summary.updated.len())
}

/// Update titles and usernames for the given channels without changing the list.
pub async fn refresh_list(
    client: &Client,
    db: &Db,
    channels: Vec<Channel>,
) -> Result<RefreshSummary> {
    if channels.is_empty() {
        return Ok(RefreshSummary {
            checked: 0,
            updated: Vec::new(),
            unavailable: Vec::new(),
        });
    }

    // A channel that is in several themes has a row in each: ask about it once.
    let mut seen = std::collections::HashSet::new();
    let channels: Vec<Channel> = channels.into_iter().filter(|c| seen.insert(c.id)).collect();

    let peers: Vec<ChannelPeer> = channels.iter().map(ChannelPeer::from).collect();
    let fresh = describe_all(client, &peers).await?;

    let mut updated = Vec::new();
    let mut unavailable = Vec::new();
    for channel in &channels {
        // The channel is not in the answer — it is not available; we keep the name from the database.
        let Some((username, title)) = fresh.get(&channel.id) else {
            unavailable.push(channel.id);
            continue;
        };
        if channel.username == *username && channel.title == *title {
            continue;
        }
        log_rename(channel, username);
        db.update_channel_names(channel.id, username.as_deref(), title)
            .await?;
        updated.push(UpdatedChannel {
            id: channel.id,
            old_title: channel.title.clone(),
            new_title: title.clone(),
            old_username: channel.username.clone(),
            new_username: username.clone(),
        });
    }

    if !updated.is_empty() {
        tracing::info!(changed = updated.len(), "channel titles updated");
    }
    Ok(RefreshSummary {
        checked: channels.len(),
        updated,
        unavailable,
    })
}

fn log_rename(channel: &Channel, username: &Option<String>) {
    if channel.username != *username {
        tracing::info!(
            id = channel.id,
            was = ?channel.username,
            now = ?username,
            "channel username changed — digest links recalculated"
        );
    }
}

/// How many channels Telegram is asked for in one `channels.getChannels` batch.
const REFRESH_BATCH_SIZE: usize = 20;

/// Pause between `channels.getChannels` batches to avoid flood limits.
const REFRESH_BATCH_PAUSE: Duration = Duration::from_secs(1);

/// Titles and usernames of many channels in small batches with a pause between them.
///
/// Channels that Telegram did not return are not in the map — the caller
/// decides what to use instead.
async fn describe_all(
    client: &Client,
    peers: &[ChannelPeer],
) -> Result<HashMap<i64, (Option<String>, String)>> {
    let mut out = HashMap::new();

    for (i, batch) in peers.chunks(REFRESH_BATCH_SIZE).enumerate() {
        if i > 0 {
            tokio::time::sleep(REFRESH_BATCH_PAUSE).await;
        }
        let id: Vec<tl::enums::InputChannel> = batch
            .iter()
            .map(|peer| {
                tl::enums::InputChannel::Channel(tl::types::InputChannel {
                    channel_id: peer.id,
                    access_hash: peer.access_hash,
                })
            })
            .collect();

        let chats = match client
            .invoke(&tl::functions::channels::GetChannels { id })
            .await
            .context("could not get data of the channels")?
        {
            tl::enums::messages::Chats::Chats(chats) => chats.chats,
            tl::enums::messages::Chats::Slice(slice) => slice.chats,
        };

        for chat in chats {
            if let Some((id, username, title)) = channel_names(&chat) {
                out.insert(id, (username, title));
            }
        }
    }

    Ok(out)
}

/// `(id, username, title)` of a channel — an unavailable channel may have no name.
fn channel_names(chat: &tl::enums::Chat) -> Option<(i64, Option<String>, String)> {
    match chat {
        tl::enums::Chat::Channel(c) => Some((c.id, active_username(c), c.title.clone())),
        tl::enums::Chat::ChannelForbidden(c) => Some((c.id, None, c.title.clone())),
        _ => None,
    }
}

/// The `@username` of a channel, or its first active name from `usernames`
/// (Telegram puts bought names only there, and leaves `username` empty).
fn active_username(channel: &tl::types::Channel) -> Option<String> {
    channel
        .username
        .as_deref()
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .or_else(|| {
            channel
                .usernames
                .as_deref()
                .into_iter()
                .flatten()
                .find_map(|u| match u {
                    tl::enums::Username::Username(u) if u.active && !u.username.is_empty() => {
                        Some(u.username.clone())
                    }
                    _ => None,
                })
        })
}

/// Mark posts as read in Telegram, up to each channel's cursor, for the personal account.
/// Reading the history through the API does not do it, so the channels would keep
/// unread counters for news the digest already told. One request per channel; a
/// failure is only logged, it never breaks the run.
pub async fn mark_read(client: &Client, db: &Db, cursors: &BTreeMap<i64, i32>) {
    let mut marked = 0;
    for (&id, &max_id) in cursors {
        let channel = match db.channel(id).await {
            Ok(Some(channel)) => channel,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), id, "could not read the channel");
                continue;
            }
        };
        let request = tl::functions::channels::ReadHistory {
            channel: tl::enums::InputChannel::Channel(tl::types::InputChannel {
                channel_id: channel.id,
                access_hash: channel.access_hash,
            }),
            max_id,
        };
        match client.invoke(&request).await {
            Ok(_) => marked += 1,
            Err(e) => tracing::warn!(
                channel = %channel.label(),
                error = %e,
                "could not mark the posts as read"
            ),
        }
    }
    if marked > 0 {
        tracing::info!(channels = marked, "posts marked as read");
    }
}

/// Id and date of the newest message in the channel, `None` if the channel has no messages.
pub async fn last_message(
    client: &Client,
    peer: ChannelPeer,
) -> Result<Option<(i32, DateTime<Utc>)>> {
    let reference = peer_ref(peer.id, peer.access_hash)?;
    let mut messages = client.iter_messages(reference).limit(1);
    let last = messages
        .next()
        .await
        .with_context(|| format!("could not read the last message of channel id {}", peer.id))?;
    Ok(last.map(|m| (m.id(), m.date())))
}

/// Id of the newest message in the channel, `None` if the channel has no messages.
pub async fn last_message_id(client: &Client, peer: ChannelPeer) -> Result<Option<i32>> {
    Ok(last_message(client, peer).await?.map(|(id, _)| id))
}

/// The read mark of the personal account in the channel: id of the last post it read.
/// `None` if the account does not follow the channel (no dialog, or it left) or read nothing.
pub async fn read_mark(client: &Client, peer: ChannelPeer) -> Result<Option<i32>> {
    let request = tl::functions::messages::GetPeerDialogs {
        peers: vec![tl::enums::InputDialogPeer::Peer(
            tl::types::InputDialogPeer {
                peer: tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
                    channel_id: peer.id,
                    access_hash: peer.access_hash,
                }),
            },
        )],
    };
    let tl::enums::messages::PeerDialogs::Dialogs(answer) = client
        .invoke(&request)
        .await
        .with_context(|| format!("could not read the dialog of channel id {}", peer.id))?;

    let following = answer
        .chats
        .iter()
        .any(|chat| matches!(chat, tl::enums::Chat::Channel(c) if c.id == peer.id && !c.left));
    if !following {
        return Ok(None);
    }
    Ok(answer.dialogs.iter().find_map(|dialog| match dialog {
        tl::enums::Dialog::Dialog(d) if d.read_inbox_max_id > 0 => Some(d.read_inbox_max_id),
        _ => None,
    }))
}

/// Title and username of one channel by peer.
pub async fn describe(client: &Client, peer: ChannelPeer) -> Result<(Option<String>, String)> {
    let reference = peer_ref(peer.id, peer.access_hash)?;
    let resolved = client
        .resolve_peer(reference)
        .await
        .with_context(|| format!("could not get data of channel id {}", peer.id))?;

    let username = match &resolved {
        Peer::Channel(c) => active_username(&c.raw),
        _ => resolved.username().map(str::to_string),
    };
    Ok((username, resolved.name().unwrap_or("no title").to_string()))
}
