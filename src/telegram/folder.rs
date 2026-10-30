//! The channel list = a Telegram folder built by hand.
//!
//! A folder (`dialogFilter`) is not only `include_peers`: it has flags for
//! automatic inclusion (`contacts`, `broadcasts`, …) and exclusion (`exclude_muted`,
//! …). An "all channels" folder (`broadcasts = true`, `include_peers = []`) looks
//! empty when read naively — and would turn off all channels at once. So
//! we support **only a folder built by hand** and check this before every
//! sync: any flag → refusal, the database stays as it is.

use anyhow::{anyhow, Context, Result};
use grammers_client::client::Client;
use grammers_client::tl;
use grammers_session::types::PeerKind;
use std::collections::HashMap;

use crate::db::{Channel, Db};
use crate::telegram::client::peer_ref;

/// A channel in the folder: `access_hash` comes together with the peer, no separate
/// resolve is needed — this is why private channels work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelPeer {
    pub id: i64,
    pub access_hash: i64,
}

/// A folder that can be used.
pub enum Folder {
    /// A normal folder without automatic rules: we read it and edit it.
    Editable(Box<tl::types::DialogFilter>),
    /// A shared folder (`dialogFilterChatlist`): the list is explicit, but we leave
    /// editing it to the owner — shared folders have their own rules.
    Shared(Vec<ChannelPeer>),
}

impl Folder {
    /// Content = `pinned_peers ∪ include_peers` minus `exclude_peers`, channels
    /// only.
    pub fn members(&self) -> Vec<ChannelPeer> {
        match self {
            Folder::Shared(peers) => peers.clone(),
            Folder::Editable(filter) => {
                let excluded: Vec<i64> = channels_of(&filter.exclude_peers)
                    .into_iter()
                    .map(|p| p.id)
                    .collect();
                let mut peers = channels_of(&filter.pinned_peers);
                for peer in channels_of(&filter.include_peers) {
                    if !peers.iter().any(|p| p.id == peer.id) {
                        peers.push(peer);
                    }
                }
                peers.retain(|p| !excluded.contains(&p.id));
                peers
            }
        }
    }
}

pub enum Lookup {
    Found(Folder),
    /// The folder exists, but it has automatic rules: we do not sync.
    Invalid(String),
    Missing,
}

/// Find the folder by name and check that it was built by hand.
pub async fn find(client: &Client, title: &str) -> Result<Lookup> {
    let tl::enums::messages::DialogFilters::Filters(filters) = client
        .invoke(&tl::functions::messages::GetDialogFilters {})
        .await
        .context("could not get the list of Telegram folders")?;

    for filter in filters.filters {
        match filter {
            tl::enums::DialogFilter::Filter(f) if filter_title(&f.title) == title => {
                return Ok(match auto_rules(&f) {
                    Some(rule) => Lookup::Invalid(format!(
                        "folder {title:?} has an automatic rule ({rule}) — remove it, \
                         sync is turned off"
                    )),
                    None => Lookup::Found(Folder::Editable(Box::new(f))),
                });
            }
            tl::enums::DialogFilter::Chatlist(f) if filter_title(&f.title) == title => {
                let mut peers = channels_of(&f.pinned_peers);
                for peer in channels_of(&f.include_peers) {
                    if !peers.iter().any(|p| p.id == peer.id) {
                        peers.push(peer);
                    }
                }
                return Ok(Lookup::Found(Folder::Shared(peers)));
            }
            _ => {}
        }
    }

    Ok(Lookup::Missing)
}

/// The first automatic inclusion or exclusion flag that is on, if there is one.
fn auto_rules(filter: &tl::types::DialogFilter) -> Option<&'static str> {
    let flags = [
        (filter.contacts, "contacts"),
        (filter.non_contacts, "non_contacts"),
        (filter.groups, "groups"),
        (filter.broadcasts, "broadcasts"),
        (filter.bots, "bots"),
        (filter.exclude_muted, "exclude_muted"),
        (filter.exclude_read, "exclude_read"),
        (filter.exclude_archived, "exclude_archived"),
    ];
    flags.iter().find(|(on, _)| *on).map(|(_, name)| *name)
}

fn filter_title(title: &tl::enums::TextWithEntities) -> &str {
    let tl::enums::TextWithEntities::Entities(t) = title;
    &t.text
}

fn channels_of(peers: &[tl::enums::InputPeer]) -> Vec<ChannelPeer> {
    peers
        .iter()
        .filter_map(|peer| match peer {
            tl::enums::InputPeer::Channel(c) => Some(ChannelPeer {
                id: c.channel_id,
                access_hash: c.access_hash,
            }),
            _ => None,
        })
        .collect()
}

/// What the sync did — we reply to `/sync` with the same text.
#[derive(Debug, Default)]
pub struct SyncReport {
    pub added: Vec<String>,
    pub disabled: Vec<String>,
    pub total: usize,
    /// The folder failed the check or does not exist: the database is not touched.
    pub refused: Option<String>,
}

/// Sync the database with the folder.
///
/// In the folder → upsert and `enabled = 1` (0 → 1 resets the cursor);
/// in the database but not in the folder → `enabled = 0`. We do not delete.
pub async fn sync(client: &Client, db: &Db, title: &str) -> Result<SyncReport> {
    let folder = match find(client, title).await? {
        Lookup::Found(folder) => folder,
        Lookup::Invalid(reason) => {
            return Ok(SyncReport {
                refused: Some(reason),
                ..Default::default()
            })
        }
        Lookup::Missing => {
            return Ok(SyncReport {
                refused: Some(format!(
                    "there is no folder {title:?} in the account — working from the database list"
                )),
                ..Default::default()
            })
        }
    };

    let members = folder.members();
    let mut report = SyncReport {
        total: members.len(),
        ..Default::default()
    };

    // We read titles and usernames again on every sync: a channel could be renamed
    // or made private, and then the links in the digest would lead nowhere.
    // This costs one request for the whole folder, not one request per channel.
    let fresh = describe_all(client, &members).await?;

    for peer in &members {
        let known = db.channel(peer.id).await?;
        // A channel that Telegram did not return (banned, deleted) keeps what we
        // already know: a lost name is no reason to lose the channel and its cursor.
        let (username, title) = match (fresh.get(&peer.id), &known) {
            (Some(fresh), _) => fresh.clone(),
            (None, Some(channel)) => (channel.username.clone(), channel.title.clone()),
            (None, None) => (None, format!("channel id {}", peer.id)),
        };

        if let Some(channel) = &known {
            log_rename(channel, &username);
        }

        if db
            .upsert_channel(peer.id, peer.access_hash, username.as_deref(), &title)
            .await?
        {
            report.added.push(format!("{title} (id {})", peer.id));
        }
    }

    let keep: Vec<i64> = members.iter().map(|p| p.id).collect();
    for channel in db.disable_channels_except(&keep).await? {
        report
            .disabled
            .push(format!("{} (id {})", channel.title, channel.id));
    }

    tracing::info!(
        folder = title,
        total = report.total,
        added = report.added.len(),
        disabled = report.disabled.len(),
        "folder synced"
    );
    Ok(report)
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
/// Needed for those who live without a folder: there `sync` is not called, and a
/// channel that was renamed or made private would stay with a stale link in the
/// digest forever. Returns the number of channels where something changed.
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

/// Titles and usernames of the whole folder in one request (per hundred channels).
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
            .context("could not get data of the folder channels")?
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

/// Add a channel to the folder. The folder is the single source of truth, so `/add`
/// edits it, not the database: otherwise the next sync would undo the edit.
pub async fn add_peer(client: &Client, folder: &Folder, peer: ChannelPeer) -> Result<()> {
    let Folder::Editable(filter) = folder else {
        return Err(anyhow!(
            "this is a shared folder (chatlist) — add the channel to it by hand in the app"
        ));
    };

    let mut filter = filter.clone();
    if !include(&mut filter, peer) {
        return Ok(());
    }
    update(client, filter).await
}

/// Add a channel to the folder content. `false` — the folder does not need a change.
///
/// We always remove the exclusion, even if the channel is already in `include_peers`:
/// `exclude_peers` is stronger, and an early return would leave `/add` with a happy answer
/// and a disabled channel.
fn include(filter: &mut tl::types::DialogFilter, peer: ChannelPeer) -> bool {
    let listed = filter
        .include_peers
        .iter()
        .chain(&filter.pinned_peers)
        .any(|p| is_channel(p, peer.id));
    let excluded = filter.exclude_peers.iter().any(|p| is_channel(p, peer.id));

    if listed && !excluded {
        return false;
    }
    filter.exclude_peers.retain(|p| !is_channel(p, peer.id));
    if !listed {
        filter.include_peers.push(input_peer(peer));
    }
    true
}

fn is_channel(peer: &tl::enums::InputPeer, id: i64) -> bool {
    matches!(peer, tl::enums::InputPeer::Channel(c) if c.channel_id == id)
}

/// Remove a channel from the folder.
pub async fn remove_peer(client: &Client, folder: &Folder, channel_id: i64) -> Result<()> {
    let Folder::Editable(filter) = folder else {
        return Err(anyhow!(
            "this is a shared folder (chatlist) — remove the channel from it by hand in the app"
        ));
    };

    let mut filter = filter.clone();
    filter.include_peers.retain(|p| !is_channel(p, channel_id));
    filter.pinned_peers.retain(|p| !is_channel(p, channel_id));

    update(client, filter).await
}

async fn update(client: &Client, filter: Box<tl::types::DialogFilter>) -> Result<()> {
    client
        .invoke(&tl::functions::messages::UpdateDialogFilter {
            id: filter.id,
            filter: Some(tl::enums::DialogFilter::Filter(*filter)),
        })
        .await
        .context("could not update the Telegram folder")?;
    Ok(())
}

fn input_peer(peer: ChannelPeer) -> tl::enums::InputPeer {
    tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
        channel_id: peer.id,
        access_hash: peer.access_hash,
    })
}

/// Copy the channels from `[[channels]]` to an empty database. After that the truth is in
/// the folder and the database, we do not look at the config any more.
pub async fn seed(
    client: &Client,
    db: &Db,
    seeds: &[&crate::config::SeedChannel],
) -> Result<usize> {
    let mut added = 0;
    for seed in seeds {
        let peer = match (&seed.username, seed.id) {
            (Some(username), _) => {
                let resolved = client
                    .resolve_username(username.trim_start_matches('@'))
                    .await
                    .with_context(|| format!("could not resolve @{username}"))?
                    .ok_or_else(|| anyhow!("channel @{username} not found"))?;
                let reference = resolved
                    .to_ref()
                    .await
                    .map_err(|e| anyhow!("channel @{username} is not available: {e}"))?
                    .ok_or_else(|| anyhow!("channel @{username} is not available"))?;
                ChannelPeer {
                    id: reference
                        .id
                        .bare_id()
                        .ok_or_else(|| anyhow!("channel @{username} has no id"))?,
                    access_hash: reference.auth.hash(),
                }
            }
            (None, Some(id)) => {
                let id = crate::db::bare_channel_id(id);
                find_in_dialogs(client, id).await?.ok_or_else(|| {
                    anyhow!(
                        "channel id {id} is not among the dialogs — does the account follow it?"
                    )
                })?
            }
            (None, None) => continue,
        };

        let (username, title) = describe(client, peer).await?;
        let title = seed.title.clone().unwrap_or(title);
        db.upsert_channel(peer.id, peer.access_hash, username.as_deref(), &title)
            .await?;
        added += 1;
        tracing::info!(channel = %title, id = peer.id, "channel copied from the config to the database");
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel_peer(id: i64) -> tl::enums::InputPeer {
        tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
            channel_id: id,
            access_hash: id * 10,
        })
    }

    fn filter(pinned: Vec<i64>, include: Vec<i64>, exclude: Vec<i64>) -> tl::types::DialogFilter {
        tl::types::DialogFilter {
            contacts: false,
            non_contacts: false,
            groups: false,
            broadcasts: false,
            bots: false,
            exclude_muted: false,
            exclude_read: false,
            exclude_archived: false,
            title_noanimate: false,
            id: 2,
            title: tl::enums::TextWithEntities::Entities(tl::types::TextWithEntities {
                text: "Digest".into(),
                entities: vec![],
            }),
            emoticon: None,
            color: None,
            pinned_peers: pinned.into_iter().map(channel_peer).collect(),
            include_peers: include.into_iter().map(channel_peer).collect(),
            exclude_peers: exclude.into_iter().map(channel_peer).collect(),
        }
    }

    #[test]
    fn members_are_pinned_plus_included_minus_excluded() {
        let folder = Folder::Editable(Box::new(filter(vec![1], vec![1, 2, 3], vec![3])));
        let ids: Vec<i64> = folder.members().iter().map(|p| p.id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn auto_rules_are_detected() {
        let mut f = filter(vec![], vec![], vec![]);
        assert_eq!(auto_rules(&f), None);

        f.broadcasts = true;
        assert_eq!(auto_rules(&f), Some("broadcasts"));

        let mut f = filter(vec![], vec![], vec![]);
        f.exclude_muted = true;
        assert_eq!(auto_rules(&f), Some("exclude_muted"));
    }

    #[test]
    fn non_channel_peers_are_ignored() {
        let mut f = filter(vec![], vec![7], vec![]);
        f.include_peers.push(tl::enums::InputPeer::PeerSelf);
        f.include_peers
            .push(tl::enums::InputPeer::Chat(tl::types::InputPeerChat {
                chat_id: 5,
            }));

        let folder = Folder::Editable(Box::new(f));
        let ids: Vec<i64> = folder.members().iter().map(|p| p.id).collect();
        assert_eq!(ids, vec![7]);
    }

    #[test]
    fn title_is_read_from_text_with_entities() {
        let f = filter(vec![], vec![], vec![]);
        assert_eq!(filter_title(&f.title), "Digest");
    }

    fn peer(id: i64) -> ChannelPeer {
        ChannelPeer {
            id,
            access_hash: id * 10,
        }
    }

    #[test]
    fn adding_a_new_channel_puts_it_into_include_peers() {
        let mut f = filter(vec![], vec![1], vec![]);
        assert!(include(&mut f, peer(2)));

        let ids: Vec<i64> = channels_of(&f.include_peers).iter().map(|p| p.id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn adding_a_channel_already_in_the_folder_changes_nothing() {
        let mut f = filter(vec![3], vec![1], vec![]);
        assert!(
            !include(&mut f, peer(1)),
            "channel already in include_peers"
        );
        assert!(!include(&mut f, peer(3)), "channel already in pinned_peers");
        assert_eq!(channels_of(&f.include_peers).len(), 1);
    }

    #[test]
    fn adding_a_channel_lifts_its_exclusion() {
        // exclude_peers is stronger than include_peers: without removing the exclusion /add
        // would answer "added", but the channel would stay disabled.
        let mut f = filter(vec![], vec![1], vec![1]);
        assert!(include(&mut f, peer(1)));
        assert!(channels_of(&f.exclude_peers).is_empty());
        assert_eq!(
            channels_of(&f.include_peers).len(),
            1,
            "there must be no duplicate"
        );

        let folder = Folder::Editable(Box::new(f));
        assert_eq!(
            folder.members().iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn excluded_channel_is_added_back_without_a_duplicate() {
        let mut f = filter(vec![], vec![], vec![5]);
        assert!(include(&mut f, peer(5)));
        assert!(channels_of(&f.exclude_peers).is_empty());
        assert_eq!(channels_of(&f.include_peers).len(), 1);
    }

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
