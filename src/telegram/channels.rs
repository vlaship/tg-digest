//! Channel lookups and name refresh.
//!
//! The channel list itself lives only in the database and is managed with `/add`
//! and `/rm` in Saved Messages. Here we find channels in Telegram and read their
//! titles and usernames again before every run.

use anyhow::{Context, Result};
use grammers_client::client::Client;
use grammers_client::tl;
use grammers_session::types::PeerKind;
use std::collections::HashMap;

use crate::db::{Channel, Db};
use crate::telegram::client::peer_ref;

/// A Telegram channel with its `access_hash`: with it we can read the channel without a
/// new resolve — this is why private channels work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelPeer {
    pub id: i64,
    pub access_hash: i64,
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

/// Update titles and usernames of the enabled channels without changing the list.
///
/// Called before every run: otherwise a channel that was renamed or made private
/// would stay with a stale link in the digest forever. Returns the number of
/// channels where something changed.
pub async fn refresh_names(client: &Client, db: &Db) -> Result<usize> {
    let channels = db.enabled_channels().await?;
    if channels.is_empty() {
        return Ok(0);
    }

    let peers: Vec<ChannelPeer> = channels
        .iter()
        .map(|c| ChannelPeer {
            id: c.id,
            access_hash: c.access_hash,
        })
        .collect();
    let fresh = describe_all(client, &peers).await?;

    let mut changed = 0;
    for channel in &channels {
        // The channel is not in the answer — it is not available; we keep the name from the database.
        let Some((username, title)) = fresh.get(&channel.id) else {
            continue;
        };
        if channel.username == *username && channel.title == *title {
            continue;
        }
        log_rename(channel, username);
        db.update_channel_names(channel.id, username.as_deref(), title)
            .await?;
        changed += 1;
    }

    if changed > 0 {
        tracing::info!(changed, "channel titles updated");
    }
    Ok(changed)
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

/// How many channels Telegram returns in one `channels.getChannels`.
const CHANNELS_PER_REQUEST: usize = 100;

/// Titles and usernames of many channels in one request (per hundred channels).
///
/// Channels that Telegram did not return are not in the map — the caller
/// decides what to use instead.
async fn describe_all(
    client: &Client,
    peers: &[ChannelPeer],
) -> Result<HashMap<i64, (Option<String>, String)>> {
    let mut out = HashMap::new();

    for batch in peers.chunks(CHANNELS_PER_REQUEST) {
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
        tl::enums::Chat::Channel(c) => Some((c.id, c.username.clone(), c.title.clone())),
        tl::enums::Chat::ChannelForbidden(c) => Some((c.id, None, c.title.clone())),
        _ => None,
    }
}

/// Title and username of one channel by peer.
pub async fn describe(client: &Client, peer: ChannelPeer) -> Result<(Option<String>, String)> {
    let reference = peer_ref(peer.id, peer.access_hash)?;
    let resolved = client
        .resolve_peer(reference)
        .await
        .with_context(|| format!("could not get data of channel id {}", peer.id))?;

    Ok((
        resolved.username().map(str::to_string),
        resolved.name().unwrap_or("no title").to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_names_are_read_from_both_chat_shapes() {
        let channel = tl::enums::Chat::Channel(tl::types::Channel {
            creator: false,
            left: false,
            broadcast: true,
            verified: false,
            megagroup: false,
            restricted: false,
            signatures: false,
            min: false,
            scam: false,
            has_link: false,
            has_geo: false,
            slowmode_enabled: false,
            call_active: false,
            call_not_empty: false,
            fake: false,
            gigagroup: false,
            noforwards: false,
            join_to_send: false,
            join_request: false,
            forum: false,
            stories_hidden: false,
            stories_hidden_min: false,
            stories_unavailable: false,
            signature_profiles: false,
            autotranslation: false,
            broadcast_messages_allowed: false,
            monoforum: false,
            forum_tabs: false,
            id: 7,
            access_hash: Some(70),
            title: "News".into(),
            username: Some("news".into()),
            photo: tl::enums::ChatPhoto::Empty,
            date: 0,
            restriction_reason: None,
            admin_rights: None,
            banned_rights: None,
            default_banned_rights: None,
            participants_count: None,
            usernames: None,
            stories_max_id: None,
            color: None,
            profile_color: None,
            emoji_status: None,
            level: None,
            subscription_until_date: None,
            bot_verification_icon: None,
            send_paid_messages_stars: None,
            linked_monoforum_id: None,
        });
        assert_eq!(
            channel_names(&channel),
            Some((7, Some("news".to_string()), "News".to_string()))
        );

        // A channel that became unavailable gives only the title — the username in the
        // database must become empty, or the links lead nowhere.
        let forbidden = tl::enums::Chat::ChannelForbidden(tl::types::ChannelForbidden {
            broadcast: true,
            megagroup: false,
            monoforum: false,
            id: 7,
            access_hash: 70,
            title: "News".into(),
            until_date: None,
        });
        assert_eq!(
            channel_names(&forbidden),
            Some((7, None, "News".to_string()))
        );

        let group = tl::enums::Chat::Chat(tl::types::Chat {
            creator: false,
            left: false,
            deactivated: false,
            call_active: false,
            call_not_empty: false,
            noforwards: false,
            id: 7,
            title: "Group".into(),
            photo: tl::enums::ChatPhoto::Empty,
            participants_count: 0,
            date: 0,
            version: 0,
            migrated_to: None,
            admin_rights: None,
            default_banned_rights: None,
        });
        assert_eq!(channel_names(&group), None);
    }
}
