//! Подключение, авторизация и выборка постов.
//!
//! ВНИМАНИЕ: grammers 0.10 переписал схему подключения по сравнению с 0.7–0.9:
//! теперь сначала создаётся `SenderPool` (session + api_id), его `runner`
//! крутится отдельной задачей, `Client::new` принимает `handle`, а updates
//! приходят отдельным приёмником пула. Сессия — SQLite-файл, а не старый
//! `Session::load_file_or_create`.
//! Сигнатуры сверять с https://docs.rs/grammers-client/0.10.0/

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use grammers_client::client::{Client, SignInError};
use grammers_client::{tl, InvocationError, SenderPool};
use grammers_session::storages::SqliteSession;
use grammers_session::types::{PeerAuth, PeerId, PeerRef};
use grammers_session::updates::UpdatesLike;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::cancel::Cancel;
use crate::config::Config;
use crate::db::Channel;
use crate::digest::Post;

/// Сколько раз повторяем чтение канала после временной ошибки.
const MAX_ATTEMPTS: u32 = 4;
/// FLOOD_WAIT дольше этого не пережидаем: канал исключается из прогона,
/// курсор не двигается, дайджест собирается по остальным.
const MAX_FLOOD_WAIT: Duration = Duration::from_secs(600);

/// Клиент вместе с приёмником updates: слушатель команд в «Избранном» берёт
/// его отсюда, потому что приёмник у пула ровно один.
pub struct Session {
    pub client: Client,
    pub updates: UnboundedReceiver<UpdatesLike>,
}

/// Подключиться к Telegram, подняв фоновую задачу сетевого пула.
pub async fn connect(cfg: &Config) -> Result<Session> {
    let session = Arc::new(
        SqliteSession::open(&cfg.telegram.session_file)
            .await
            .with_context(|| format!("не удалось открыть сессию {}", cfg.telegram.session_file))?,
    );

    let pool = SenderPool::new(session, cfg.telegram.api_id);
    let SenderPool {
        runner,
        handle,
        updates,
    } = pool;

    // Пул сам по себе ничего не делает — его надо крутить.
    tokio::spawn(runner.run());

    Ok(Session {
        client: Client::new(handle),
        updates,
    })
}

/// Интерактивный вход: код из Telegram + пароль 2FA, если он включён.
pub async fn login(client: &Client, cfg: &Config) -> Result<()> {
    if client.is_authorized().await? {
        println!("Уже авторизованы, повторный вход не нужен.");
        return Ok(());
    }

    let phone = &cfg.telegram.phone;
    println!("Запрашиваем код для {phone}…");

    let token = client
        .request_login_code(phone, &cfg.telegram.api_hash)
        .await
        .context("не удалось запросить код входа")?;

    let code = prompt("Код из Telegram: ")?;

    match client.sign_in(&token, code.trim()).await {
        Ok(user) => {
            println!("Готово, вошли как {}", user.id());
            Ok(())
        }
        // Без этой ветки аккаунты с двухфакторной авторизацией не залогинятся.
        Err(SignInError::PasswordRequired(password_token)) => {
            let password = rpassword::prompt_password("Пароль 2FA: ")
                .context("не удалось прочитать пароль")?;
            let user = client
                .check_password(password_token, password.trim())
                .await
                .context("неверный пароль 2FA")?;
            println!("Готово, вошли как {}", user.id());
            Ok(())
        }
        Err(e) => Err(anyhow!("вход не удался: {e}")),
    }
}

pub async fn ensure_authorized(client: &Client) -> Result<()> {
    if !client.is_authorized().await? {
        bail!("нет авторизации в Telegram — выполните `tg-digest login`");
    }
    Ok(())
}

/// Сессия отозвана («Завершить все другие сеансы», удалённый аккаунт).
///
/// Это не «ошибка одного прогона»: пережидать её бессмысленно, поэтому демон
/// на такой ошибке выходит с ненулевым кодом и просит выполнить `login`.
pub fn is_auth_error(error: &InvocationError) -> bool {
    const FATAL: [&str; 5] = [
        "AUTH_KEY_UNREGISTERED",
        "AUTH_KEY_DUPLICATED",
        "SESSION_REVOKED",
        "SESSION_EXPIRED",
        "USER_DEACTIVATED",
    ];
    matches!(error, InvocationError::Rpc(rpc) if FATAL.iter().any(|name| rpc.is(name)))
}

/// То же самое, но для ошибки, уже завёрнутой в `anyhow`: прогон таких не
/// переживает, демон обязан выйти.
pub fn is_fatal(error: &anyhow::Error) -> bool {
    error.chain().any(|e| {
        e.downcast_ref::<InvocationError>()
            .is_some_and(is_auth_error)
    })
}

/// Сколько просит подождать FLOOD_WAIT, если это он.
pub fn flood_wait(error: &InvocationError) -> Option<Duration> {
    match error {
        InvocationError::Rpc(rpc) if rpc.is("FLOOD_WAIT") || rpc.is("FLOOD_PREMIUM_WAIT") => {
            Some(Duration::from_secs(rpc.value.unwrap_or(0) as u64))
        }
        _ => None,
    }
}

pub fn peer_ref(id: i64, access_hash: i64) -> Result<PeerRef> {
    let peer_id =
        PeerId::channel(id).ok_or_else(|| anyhow!("{id} не похож на идентификатор канала"))?;
    Ok(PeerRef {
        id: peer_id,
        auth: PeerAuth::from_hash(access_hash),
    })
}

/// Результат сбора по всем каналам.
pub struct Collected {
    pub posts: Vec<Post>,
    /// Курсоры **успешно прочитанных** каналов: максимальный id, который видели,
    /// независимо от фильтра по длине и склейки альбомов.
    pub cursors: BTreeMap<i64, i32>,
    /// Каналы, исключённые из прогона, и причина. Курсор у них не двигается.
    pub failures: Vec<(Channel, String)>,
}

/// Собрать посты включённых каналов.
///
/// Граница — курсор канала, а не глобальное время: канал, который три дня
/// падал, при восстановлении отдаст все три дня. Дата участвует только при
/// курсоре 0 (новый или сброшенный канал).
pub async fn collect_posts(
    client: &Client,
    cfg: &Config,
    channels: &[Channel],
    cancel: &Cancel,
) -> Result<Collected> {
    let since = Utc::now() - ChronoDuration::hours(cfg.telegram.lookback_hours);
    let pause = Duration::from_millis(cfg.telegram.pause_between_channels_ms);

    let mut collected = Collected {
        posts: Vec::new(),
        cursors: BTreeMap::new(),
        failures: Vec::new(),
    };

    for (index, channel) in channels.iter().enumerate() {
        if cancel.is_cancelled() {
            bail!("сбор остановлен сигналом");
        }
        if index > 0 {
            // Пауза между каналами — самая дешёвая защита от FLOOD_WAIT.
            if cancel.sleep(pause).await.is_err() {
                bail!("сбор остановлен сигналом");
            }
        }

        match fetch_channel(client, cfg, channel, since, cancel).await {
            Ok(batch) => {
                tracing::info!(
                    channel = %channel.label(),
                    count = batch.posts.len(),
                    cursor = batch.max_id_seen,
                    "собрано постов"
                );
                if batch.max_id_seen > 0 {
                    collected.cursors.insert(channel.id, batch.max_id_seen);
                }
                collected
                    .posts
                    .extend(batch.posts.into_iter().map(|p| Post {
                        channel_id: channel.id,
                        channel: channel.label(),
                        title: channel.title.clone(),
                        date: p.date,
                        url: channel.post_url(p.msg_id),
                        origin: p.forward.as_ref().map(|f| describe_origin(f, channels)),
                        text: p.text,
                    }));
            }
            Err(ChannelError::Cancelled) => bail!("сбор остановлен сигналом"),
            Err(ChannelError::Auth(e)) => return Err(e).context("сессия Telegram недействительна"),
            Err(ChannelError::Excluded(reason)) => {
                tracing::warn!(channel = %channel.label(), %reason, "канал исключён из прогона");
                collected.failures.push((channel.clone(), reason));
            }
        }
    }

    Ok(collected)
}

enum ChannelError {
    /// Канал выпадает из этого прогона; курсор не двигаем, дайджест собираем
    /// по остальным.
    Excluded(String),
    Auth(anyhow::Error),
    Cancelled,
}

async fn fetch_channel(
    client: &Client,
    cfg: &Config,
    channel: &Channel,
    since: DateTime<Utc>,
    cancel: &Cancel,
) -> Result<Batch, ChannelError> {
    let peer = peer_ref(channel.id, channel.access_hash)
        .map_err(|e| ChannelError::Excluded(e.to_string()))?;

    let limit = cfg.telegram.max_posts_per_channel;
    // +1: чтобы отличить «канал кончился ровно на лимите» от «упёрлись в лимит».
    let mut iter = client.iter_messages(peer).limit(limit + 1);
    let mut raw: Vec<RawMessage> = Vec::new();
    let mut attempt = 0;

    loop {
        if cancel.is_cancelled() {
            return Err(ChannelError::Cancelled);
        }

        let result = tokio::select! {
            _ = cancel.cancelled() => return Err(ChannelError::Cancelled),
            result = iter.next() => result,
        };
        let next = match result {
            Ok(message) => {
                attempt = 0;
                message
            }
            Err(e) if is_auth_error(&e) => return Err(ChannelError::Auth(e.into())),
            Err(e) => {
                attempt += 1;
                let wait = flood_wait(&e);
                if attempt >= MAX_ATTEMPTS || wait.is_some_and(|w| w > MAX_FLOOD_WAIT) {
                    return Err(ChannelError::Excluded(format!(
                        "ошибка чтения канала: {e}. Следующий прогон дочитает с курсора"
                    )));
                }
                let delay = wait.unwrap_or_else(|| Duration::from_secs(2u64.pow(attempt)));
                tracing::warn!(channel = %channel.label(), error = %e, ?delay, "повтор чтения");
                if cancel.sleep(delay).await.is_err() {
                    return Err(ChannelError::Cancelled);
                }
                continue;
            }
        };

        let Some(message) = next else { break };

        if reached_border(message.id(), message.date(), channel.last_msg_id, since) {
            break;
        }

        // Упёрлись в предохранитель, не дойдя до курсора: молча отрезать хвост
        // нельзя, поэтому канал выпадает из прогона целиком.
        if raw.len() >= limit {
            return Err(ChannelError::Excluded(format!(
                "превышен лимит {limit} постов за прогон — поднимите telegram.max_posts_per_channel \
                 или сбросьте канал через /reset {}",
                channel.id
            )));
        }

        raw.push(RawMessage {
            id: message.id(),
            date: message.date(),
            grouped_id: message.grouped_id(),
            text: with_hidden_links(message.text(), message.fmt_entities()),
            forward: forward_origin(message.forward_header().as_ref()),
        });
    }

    Ok(assemble(&raw, cfg.telegram.min_post_chars))
}

/// Граница сбора. При курсоре 0 (новый или сброшенный канал) берём окно по
/// дате, иначе читаем всё, что выше курсора, сколько бы дней это ни было.
fn reached_border(id: i32, date: DateTime<Utc>, cursor: i32, since: DateTime<Utc>) -> bool {
    if cursor > 0 {
        id <= cursor
    } else {
        date < since
    }
}

/// Сообщение до склейки альбомов и фильтра — в таком виде его удобно гонять
/// в тестах без Telegram.
#[derive(Debug, Clone)]
pub struct RawMessage {
    pub id: i32,
    pub date: DateTime<Utc>,
    pub grouped_id: Option<i64>,
    pub text: String,
    /// Откуда репост, если это репост.
    pub forward: Option<Forward>,
}

/// Происхождение репоста. Нужно, чтобы слияние дублей не считало репост и
/// оригинал двумя разными новостями: без пометки модель честно выдаёт два
/// пункта об одном событии.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Forward {
    /// Имя исходного канала, если Telegram его прислал (у скрытых — нет).
    pub name: Option<String>,
    /// Id исходного канала: по нему ищем название среди своих каналов.
    pub channel_id: Option<i64>,
    /// Номер поста в исходном канале.
    pub post_id: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct PreparedPost {
    pub msg_id: i32,
    pub date: DateTime<Utc>,
    pub text: String,
    pub forward: Option<Forward>,
}

pub struct Batch {
    pub posts: Vec<PreparedPost>,
    /// Максимальный id, который видели, **до** фильтра: иначе части альбома
    /// остаются выше курсора и на следующем прогоне читаются заново.
    pub max_id_seen: i32,
}

/// Склейка альбомов и фильтр коротких постов.
///
/// Альбом = несколько сообщений с одним `grouped_id`, но одна публикация:
/// текст лежит в подписи к одной из частей, ссылку даём на самую раннюю.
pub fn assemble(raw: &[RawMessage], min_post_chars: usize) -> Batch {
    let mut albums: HashMap<i64, PreparedPost> = HashMap::new();
    let mut posts: Vec<PreparedPost> = Vec::new();
    let mut max_id_seen = 0;

    for message in raw {
        max_id_seen = max_id_seen.max(message.id);
        let text = message.text.trim().to_string();

        match message.grouped_id {
            Some(group) => {
                let entry = albums.entry(group).or_insert_with(|| PreparedPost {
                    msg_id: message.id,
                    date: message.date,
                    text: String::new(),
                    forward: None,
                });
                if entry.text.is_empty() && !text.is_empty() {
                    entry.text = text;
                }
                // Альбом целиком — одна пересылка, так что источник берём
                // у любой части, у которой он есть.
                if entry.forward.is_none() {
                    entry.forward = message.forward.clone();
                }
                // Ссылка должна вести на первую часть альбома.
                if message.id < entry.msg_id {
                    entry.msg_id = message.id;
                    entry.date = message.date;
                }
            }
            None => posts.push(PreparedPost {
                msg_id: message.id,
                date: message.date,
                text,
                forward: message.forward.clone(),
            }),
        }
    }

    posts.extend(albums.into_values());

    // Короткие посты — это стикеры, реакции и «только медиа»: для дайджеста шум.
    posts.retain(|p| p.text.chars().count() >= min_post_chars);
    posts.sort_by_key(|p| p.msg_id);

    Batch { posts, max_id_seen }
}

/// Человекочитаемый источник репоста.
///
/// Если исходный канал — один из наших, берём его название и ссылку на
/// оригинальный пост: тогда модель видит, что репост и оригинал — одно
/// событие, а не два.
fn describe_origin(forward: &Forward, channels: &[Channel]) -> String {
    let known = forward
        .channel_id
        .and_then(|id| channels.iter().find(|c| c.id == id));

    match (known, &forward.name, forward.post_id) {
        (Some(channel), _, Some(post_id)) => {
            format!("{} ({})", channel.title, channel.post_url(post_id))
        }
        (Some(channel), _, None) => channel.title.clone(),
        (None, Some(name), _) => name.clone(),
        (None, None, _) => match forward.channel_id {
            Some(id) => format!("канал id {id}"),
            None => "скрытый источник".into(),
        },
    }
}

/// Откуда репост. У скрытых пересылок нет ни id, ни имени — тогда `None`.
fn forward_origin(header: Option<&tl::enums::MessageFwdHeader>) -> Option<Forward> {
    let tl::enums::MessageFwdHeader::Header(header) = header?;

    let channel_id = match &header.from_id {
        Some(tl::enums::Peer::Channel(peer)) => Some(peer.channel_id),
        _ => None,
    };
    let forward = Forward {
        name: header.from_name.clone(),
        channel_id,
        post_id: header.channel_post,
    };

    (forward != Forward::default()).then_some(forward)
}

/// `message.text()` отдаёт голый текст: ссылка из «подробнее — [тут]» пропадает.
/// Дописываем такие адреса отдельной строкой — модели они нужны для контекста,
/// а в `sources` всё равно попадут только ссылки на сами посты.
fn with_hidden_links(text: &str, entities: Option<&Vec<tl::enums::MessageEntity>>) -> String {
    let Some(entities) = entities else {
        return text.to_string();
    };

    let mut urls: Vec<&str> = entities
        .iter()
        .filter_map(|entity| match entity {
            tl::enums::MessageEntity::TextUrl(e) => Some(e.url.as_str()),
            _ => None,
        })
        .filter(|url| !text.contains(*url))
        .collect();
    urls.dedup();

    if urls.is_empty() {
        return text.to_string();
    }
    format!("{text}\nСсылки в посте: {}", urls.join(", "))
}

fn prompt(label: &str) -> Result<String> {
    use std::io::{stdin, stdout, Write};
    print!("{label}");
    stdout().flush()?;
    let mut buf = String::new();
    stdin().read_line(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + ChronoDuration::minutes(minutes)
    }

    fn message(id: i32, group: Option<i64>, text: &str) -> RawMessage {
        RawMessage {
            id,
            date: at(id as i64),
            grouped_id: group,
            text: text.into(),
            forward: None,
        }
    }

    fn forwarded(id: i32, group: Option<i64>, text: &str, from: Forward) -> RawMessage {
        RawMessage {
            forward: Some(from),
            ..message(id, group, text)
        }
    }

    fn channel(id: i64, username: Option<&str>, title: &str) -> Channel {
        Channel {
            id,
            access_hash: 1,
            username: username.map(str::to_string),
            title: title.into(),
            enabled: true,
            last_msg_id: 0,
            last_error: None,
        }
    }

    #[test]
    fn album_parts_become_one_post_linked_to_the_first() {
        let raw = vec![
            message(12, Some(7), ""),
            message(11, Some(7), "подпись под альбомом, достаточно длинная"),
            message(10, Some(7), ""),
        ];
        let batch = assemble(&raw, 10);
        assert_eq!(batch.posts.len(), 1);
        assert_eq!(batch.posts[0].msg_id, 10);
        assert_eq!(batch.max_id_seen, 12);
    }

    #[test]
    fn cursor_counts_filtered_out_messages_too() {
        // Альбом без подписи и стикер отфильтруются, но курсор обязан уйти за
        // них: иначе следующий прогон перечитает альбом и выдаст его дважды.
        let raw = vec![
            message(30, Some(1), ""),
            message(29, Some(1), ""),
            message(28, None, "ok"),
            message(27, None, "достаточно длинный пост, чтобы пройти фильтр"),
        ];
        let batch = assemble(&raw, 10);
        assert_eq!(batch.posts.len(), 1);
        assert_eq!(batch.posts[0].msg_id, 27);
        assert_eq!(batch.max_id_seen, 30);
    }

    #[test]
    fn posts_come_out_oldest_first() {
        let raw = vec![
            message(3, None, "третий пост, длинный достаточно"),
            message(1, None, "первый пост, длинный достаточно"),
            message(2, None, "второй пост, длинный достаточно"),
        ];
        let batch = assemble(&raw, 10);
        let ids: Vec<i32> = batch.posts.iter().map(|p| p.msg_id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn empty_channel_leaves_the_cursor_alone() {
        let batch = assemble(&[], 10);
        assert_eq!(batch.max_id_seen, 0);
        assert!(batch.posts.is_empty());
    }

    #[test]
    fn border_is_the_cursor_when_there_is_one() {
        let since = at(100);
        // Старый по дате, но выше курсора — берём: канал мог три дня падать.
        assert!(!reached_border(50, at(0), 40, since));
        assert!(reached_border(40, at(200), 40, since));
    }

    #[test]
    fn border_is_the_date_window_for_a_fresh_channel() {
        let since = at(100);
        assert!(!reached_border(50, at(120), 0, since));
        assert!(reached_border(50, at(90), 0, since));
    }

    #[test]
    fn hidden_link_is_appended_once() {
        let entities = vec![
            tl::enums::MessageEntity::TextUrl(tl::types::MessageEntityTextUrl {
                offset: 0,
                length: 4,
                url: "https://example.com/a".into(),
            }),
            tl::enums::MessageEntity::Bold(tl::types::MessageEntityBold {
                offset: 0,
                length: 4,
            }),
        ];
        let out = with_hidden_links("тут", Some(&entities));
        assert!(out.contains("https://example.com/a"));

        // Адрес, который и так виден в тексте, второй раз не дописываем.
        let out = with_hidden_links("см. https://example.com/a", Some(&entities));
        assert_eq!(out, "см. https://example.com/a");
    }

    #[test]
    fn repost_keeps_the_source_channel() {
        let raw = vec![forwarded(
            5,
            None,
            "новость, пересказанная достаточно длинно",
            Forward {
                name: None,
                channel_id: Some(777),
                post_id: Some(42),
            },
        )];
        let batch = assemble(&raw, 10);
        assert_eq!(
            batch.posts[0].forward.as_ref().unwrap().channel_id,
            Some(777)
        );
    }

    #[test]
    fn album_takes_the_source_from_whichever_part_has_it() {
        let from = Forward {
            name: Some("Первоисточник".into()),
            channel_id: None,
            post_id: None,
        };
        let raw = vec![
            message(12, Some(7), ""),
            forwarded(
                11,
                Some(7),
                "подпись под альбомом, достаточно длинная",
                from,
            ),
            message(10, Some(7), ""),
        ];
        let batch = assemble(&raw, 10);
        assert_eq!(batch.posts.len(), 1);
        assert_eq!(
            batch.posts[0].forward.as_ref().unwrap().name.as_deref(),
            Some("Первоисточник")
        );
    }

    #[test]
    fn a_repost_from_a_tracked_channel_points_at_the_original_post() {
        let channels = vec![channel(777, Some("source"), "Первоисточник")];
        let origin = describe_origin(
            &Forward {
                name: None,
                channel_id: Some(777),
                post_id: Some(42),
            },
            &channels,
        );
        assert_eq!(origin, "Первоисточник (https://t.me/source/42)");
    }

    #[test]
    fn a_repost_from_an_unknown_channel_falls_back_to_whatever_telegram_gave() {
        let channels = vec![channel(1, None, "Наш канал")];

        assert_eq!(
            describe_origin(
                &Forward {
                    name: Some("Чужой канал".into()),
                    channel_id: Some(999),
                    post_id: Some(1),
                },
                &channels
            ),
            "Чужой канал"
        );
        assert_eq!(
            describe_origin(
                &Forward {
                    name: None,
                    channel_id: Some(999),
                    post_id: None,
                },
                &channels
            ),
            "канал id 999"
        );
        assert_eq!(
            describe_origin(&Forward::default(), &channels),
            "скрытый источник"
        );
    }

    #[test]
    fn a_message_without_a_forward_header_has_no_origin() {
        assert_eq!(forward_origin(None), None);

        // Полностью скрытая пересылка: ни имени, ни id — помечать нечем.
        let empty = tl::enums::MessageFwdHeader::Header(tl::types::MessageFwdHeader {
            imported: false,
            saved_out: false,
            from_id: None,
            from_name: None,
            date: 0,
            channel_post: None,
            post_author: None,
            saved_from_peer: None,
            saved_from_msg_id: None,
            saved_from_id: None,
            saved_from_name: None,
            saved_date: None,
            psa_type: None,
        });
        assert_eq!(forward_origin(Some(&empty)), None);
    }
}
