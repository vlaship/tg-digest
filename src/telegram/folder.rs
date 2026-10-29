//! Список каналов = вручную собранная папка в Telegram.
//!
//! Папка (`dialogFilter`) — это не только `include_peers`: у неё есть флаги
//! автовключения (`contacts`, `broadcasts`, …) и исключения (`exclude_muted`,
//! …). Папка «все каналы» (`broadcasts = true`, `include_peers = []`) при
//! наивном чтении выглядит пустой — и выключила бы все каналы разом. Поэтому
//! поддерживаем **только явно собранную папку** и проверяем это перед каждым
//! синхронизацией: любой флаг → отказ, база остаётся как есть.

use anyhow::{anyhow, Context, Result};
use grammers_client::client::Client;
use grammers_client::tl;
use grammers_session::types::PeerKind;
use std::collections::HashMap;

use crate::db::{Channel, Db};
use crate::telegram::client::peer_ref;

/// Канал в папке: `access_hash` приходит вместе с пиром, отдельный резолв не
/// нужен — за счёт этого работают приватные каналы.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelPeer {
    pub id: i64,
    pub access_hash: i64,
}

/// Папка, пригодная к использованию.
pub enum Folder {
    /// Обычная папка без авто-правил: и читаем, и правим.
    Editable(Box<tl::types::DialogFilter>),
    /// Общая папка (`dialogFilterChatlist`): состав явный, но правку её списка
    /// оставляем владельцу — у общих папок свои правила.
    Shared(Vec<ChannelPeer>),
}

impl Folder {
    /// Состав = `pinned_peers ∪ include_peers` минус `exclude_peers`, только
    /// каналы.
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
    /// Папка есть, но с авто-правилами: синхронизацию не выполняем.
    Invalid(String),
    Missing,
}

/// Найти папку по имени и проверить, что она собрана руками.
pub async fn find(client: &Client, title: &str) -> Result<Lookup> {
    let tl::enums::messages::DialogFilters::Filters(filters) = client
        .invoke(&tl::functions::messages::GetDialogFilters {})
        .await
        .context("не удалось получить список папок Telegram")?;

    for filter in filters.filters {
        match filter {
            tl::enums::DialogFilter::Filter(f) if filter_title(&f.title) == title => {
                return Ok(match auto_rules(&f) {
                    Some(rule) => Lookup::Invalid(format!(
                        "папка {title:?} содержит авто-правило ({rule}) — уберите его, \
                         синхронизация отключена"
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

/// Первый включённый флаг автовключения или исключения, если он есть.
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

/// Что дал sync — этим же текстом отвечаем на `/sync`.
#[derive(Debug, Default)]
pub struct SyncReport {
    pub added: Vec<String>,
    pub disabled: Vec<String>,
    pub total: usize,
    /// Папка не прошла проверку или её нет: база не тронута.
    pub refused: Option<String>,
}

/// Синхронизировать базу с папкой.
///
/// Есть в папке → upsert и `enabled = 1` (0 → 1 сбрасывает курсор);
/// есть в базе, нет в папке → `enabled = 0`. Не удаляем.
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
                    "папки {title:?} в аккаунте нет — работаем по списку из базы"
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

    // Названия и username перечитываем на каждом sync: канал могли переименовать
    // или увести в приватные, и тогда ссылки в дайджесте вели бы в никуда.
    // Стоит это один запрос на всю папку, а не по запросу на канал.
    let fresh = describe_all(client, &members).await?;

    for peer in &members {
        let known = db.channel(peer.id).await?;
        // Канал, которого Telegram не вернул (забанен, удалён), оставляем с тем,
        // что уже знаем: пропажа имени — не повод терять канал и его курсор.
        let (username, title) = match (fresh.get(&peer.id), &known) {
            (Some(fresh), _) => fresh.clone(),
            (None, Some(channel)) => (channel.username.clone(), channel.title.clone()),
            (None, None) => (None, format!("канал id {}", peer.id)),
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
        "папка синхронизирована"
    );
    Ok(report)
}

/// Приватный канал по числовому id: `access_hash` у нас его нет, а без него
/// `channels.getChannels` не ответит. Берём пир из списка диалогов — он там
/// есть, если аккаунт на канал подписан.
pub async fn find_in_dialogs(client: &Client, id: i64) -> Result<Option<ChannelPeer>> {
    let mut dialogs = client.iter_dialogs();
    while let Some(dialog) = dialogs
        .next()
        .await
        .context("не удалось прочитать список диалогов")?
    {
        let reference = dialog.peer_ref();
        // Тип пира проверяем обязательно: «голые» id у пользователя, группы и
        // канала живут в разных пространствах и вполне могут совпасть, а
        // читать мы умеем только каналы.
        if reference.id.kind() == PeerKind::Channel && reference.id.bare_id() == Some(id) {
            return Ok(Some(ChannelPeer {
                id,
                access_hash: reference.auth.hash(),
            }));
        }
    }
    Ok(None)
}

/// Обновить названия и username включённых каналов, не трогая состав.
///
/// Нужно тем, кто живёт без папки: там `sync` не вызывается, и канал,
/// переименованный или ушедший в приватные, навсегда остался бы со старой
/// ссылкой в дайджесте. Возвращает число каналов, у которых что-то изменилось.
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
        // Канала нет в ответе — он недоступен; имя из базы не трогаем.
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
        tracing::info!(changed, "названия каналов обновлены");
    }
    Ok(changed)
}

fn log_rename(channel: &Channel, username: &Option<String>) {
    if channel.username != *username {
        tracing::info!(
            id = channel.id,
            was = ?channel.username,
            now = ?username,
            "у канала сменился username — ссылки в дайджесте пересчитаны"
        );
    }
}

/// Сколько каналов Telegram отдаёт за один `channels.getChannels`.
const CHANNELS_PER_REQUEST: usize = 100;

/// Названия и username всей папки одним запросом (на каждую сотню каналов).
///
/// Каналы, которых Telegram не вернул, в карту не попадают — вызывающая сторона
/// решает, чем их заменить.
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
            .context("не удалось получить данные каналов папки")?
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

/// `(id, username, title)` канала — у недоступного канала имени может не быть.
fn channel_names(chat: &tl::enums::Chat) -> Option<(i64, Option<String>, String)> {
    match chat {
        tl::enums::Chat::Channel(c) => Some((c.id, c.username.clone(), c.title.clone())),
        tl::enums::Chat::ChannelForbidden(c) => Some((c.id, None, c.title.clone())),
        _ => None,
    }
}

/// Название и username одного канала по пиру.
pub async fn describe(client: &Client, peer: ChannelPeer) -> Result<(Option<String>, String)> {
    let reference = peer_ref(peer.id, peer.access_hash)?;
    let resolved = client
        .resolve_peer(reference)
        .await
        .with_context(|| format!("не удалось получить данные канала id {}", peer.id))?;

    Ok((
        resolved.username().map(str::to_string),
        resolved.name().unwrap_or("без названия").to_string(),
    ))
}

/// Добавить канал в папку. Единый источник правды — папка, поэтому `/add`
/// правит её, а не базу: иначе следующий sync откатил бы правку.
pub async fn add_peer(client: &Client, folder: &Folder, peer: ChannelPeer) -> Result<()> {
    let Folder::Editable(filter) = folder else {
        return Err(anyhow!(
            "это общая папка (chatlist) — добавьте канал в неё вручную, из приложения"
        ));
    };

    let mut filter = filter.clone();
    if !include(&mut filter, peer) {
        return Ok(());
    }
    update(client, filter).await
}

/// Добавить канал в состав папки. `false` — папку менять не нужно.
///
/// Исключение снимаем всегда, даже если канал уже лежит в `include_peers`:
/// `exclude_peers` сильнее, и ранний выход оставил бы `/add` с бодрым ответом
/// и выключенным каналом.
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

/// Убрать канал из папки.
pub async fn remove_peer(client: &Client, folder: &Folder, channel_id: i64) -> Result<()> {
    let Folder::Editable(filter) = folder else {
        return Err(anyhow!(
            "это общая папка (chatlist) — уберите канал из неё вручную, из приложения"
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
        .context("не удалось обновить папку Telegram")?;
    Ok(())
}

fn input_peer(peer: ChannelPeer) -> tl::enums::InputPeer {
    tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
        channel_id: peer.id,
        access_hash: peer.access_hash,
    })
}

/// Занести каналы из `[[channels]]` в пустую базу. Дальше правда — в папке и
/// в базе, конфиг больше не смотрим.
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
                    .with_context(|| format!("не удалось разрешить @{username}"))?
                    .ok_or_else(|| anyhow!("канал @{username} не найден"))?;
                let reference = resolved
                    .to_ref()
                    .await
                    .map_err(|e| anyhow!("канал @{username} недоступен: {e}"))?
                    .ok_or_else(|| anyhow!("канал @{username} недоступен"))?;
                ChannelPeer {
                    id: reference
                        .id
                        .bare_id()
                        .ok_or_else(|| anyhow!("у канала @{username} нет идентификатора"))?,
                    access_hash: reference.auth.hash(),
                }
            }
            (None, Some(id)) => {
                let id = crate::db::bare_channel_id(id);
                find_in_dialogs(client, id).await?.ok_or_else(|| {
                    anyhow!("канала id {id} нет среди диалогов — аккаунт на него подписан?")
                })?
            }
            (None, None) => continue,
        };

        let (username, title) = describe(client, peer).await?;
        let title = seed.title.clone().unwrap_or(title);
        db.upsert_channel(peer.id, peer.access_hash, username.as_deref(), &title)
            .await?;
        added += 1;
        tracing::info!(channel = %title, id = peer.id, "канал перенесён из конфига в базу");
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
        assert!(!include(&mut f, peer(1)), "канал уже в include_peers");
        assert!(!include(&mut f, peer(3)), "канал уже в pinned_peers");
        assert_eq!(channels_of(&f.include_peers).len(), 1);
    }

    #[test]
    fn adding_a_channel_lifts_its_exclusion() {
        // exclude_peers сильнее include_peers: без снятия исключения /add
        // отвечал бы «добавлен», а канал оставался бы выключенным.
        let mut f = filter(vec![], vec![1], vec![1]);
        assert!(include(&mut f, peer(1)));
        assert!(channels_of(&f.exclude_peers).is_empty());
        assert_eq!(
            channels_of(&f.include_peers).len(),
            1,
            "дубля быть не должно"
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

        // Канал, ставший недоступным, отдаёт только название — username в базе
        // должен обнулиться, иначе ссылки поведут в никуда.
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
