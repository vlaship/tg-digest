//! Отправка дайджеста и журнал доставки.
//!
//! Идемпотентность даёт сам Telegram: `messages.sendMessage` принимает
//! `random_id` «для предотвращения повторной отправки». `random_id` выдаётся
//! один раз при записи части в журнал и переиспользуется во всех повторах,
//! включая повторы после рестарта контейнера. Срок серверного окна дедупликации
//! не документирован, поэтому это снижение риска, а не гарантия: за пределами
//! `publish_retry_window_hours` повтор делает только владелец (`/resume`).
//!
//! Высокоуровневый `Client::send_message` `random_id` не принимает — шлём через
//! `invoke(messages::SendMessage { … })`, разметку берём из того же HTML-парсера.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use grammers_client::client::Client;
use grammers_client::parsers::parse_html_message;
use grammers_client::{tl, InvocationError};
use grammers_session::types::{PeerAuth, PeerId, PeerRef};
use std::time::Duration;

use crate::cancel::Cancel;
use crate::db::{Db, Destination, Run};
use crate::telegram::client::{flood_wait, is_auth_error};

/// Пауза между частями длинного дайджеста, чтобы Telegram не считал это флудом.
const PAUSE_BETWEEN_CHUNKS: Duration = Duration::from_millis(700);
/// Сколько раз повторяем часть с неясным исходом, прежде чем оставить прогон
/// в статусе `publishing` и позвать владельца.
const MAX_ATTEMPTS: u32 = 4;

/// Исход одной попытки отправки. Разделение на «неясно» и «отказ» — главное:
/// неясный исход повторяется с тем же `random_id`, отказ повторять бесполезно.
#[derive(Debug)]
pub enum SendOutcome {
    /// Часть у Telegram есть. `sent_msg_id` может отсутствовать — для части
    /// пиров id в ответе не приходит, и это нормально.
    Delivered { sent_msg_id: Option<i32> },
    /// Таймаут, обрыв, ошибка без кода: в базу ничего не пишем, повторяем.
    Unclear(String),
    /// FLOOD_WAIT: столько просят подождать.
    Wait(Duration, String),
    /// `CHAT_WRITE_FORBIDDEN`, `PEER_ID_INVALID` и прочее, что повтор не лечит.
    Rejected(String),
    /// Сессия недействительна — это не ошибка прогона, а конец работы демона.
    Auth(InvocationError),
}

/// Куда постим: `"me"` — «Избранное» текущего аккаунта, иначе `@username`.
///
/// Резолвится один раз, при записи частей в журнал: смена `destination` в
/// конфиге и рестарт не уводят хвост недоставленного дайджеста в другой чат.
pub async fn resolve_destination(client: &Client, destination: &str) -> Result<Destination> {
    let peer = if destination == "me" {
        client
            .get_me()
            .await
            .context("не удалось получить свой профиль")?
            .to_ref()
            .await
            .map_err(|e| anyhow!("не удалось получить ссылку на свой профиль: {e}"))?
            .ok_or_else(|| anyhow!("свой профиль недоступен для отправки"))?
    } else {
        let username = destination.trim_start_matches('@');
        client
            .resolve_username(username)
            .await
            .with_context(|| format!("не удалось разрешить {destination}"))?
            .ok_or_else(|| anyhow!("канал {destination} не найден"))?
            .to_ref()
            .await
            .map_err(|e| anyhow!("не удалось получить ссылку на {destination}: {e}"))?
            .ok_or_else(|| anyhow!("канал {destination} недоступен для отправки"))?
    };

    Ok(Destination {
        peer_id: peer
            .id
            .bot_api_dialog_id()
            .ok_or_else(|| anyhow!("у адресата {destination} нет идентификатора"))?,
        access_hash: peer.auth.hash(),
    })
}

pub fn destination_ref(dest: Destination) -> Result<PeerRef> {
    let id = PeerId::from_bot_api_dialog_id(dest.peer_id)
        .ok_or_else(|| anyhow!("{} не похож на идентификатор чата", dest.peer_id))?;
    Ok(PeerRef {
        id,
        auth: PeerAuth::from_hash(dest.access_hash),
    })
}

/// Одна попытка отправки части.
pub async fn send_chunk(client: &Client, peer: PeerRef, html: &str, random_id: i64) -> SendOutcome {
    let (message, entities) = parse_html_message(html);

    let request = tl::functions::messages::SendMessage {
        // link_preview выключен: иначе Telegram растянет превью первой ссылки
        // источника на весь пост и дайджест поедет.
        no_webpage: true,
        silent: false,
        background: false,
        clear_draft: false,
        noforwards: false,
        update_stickersets_order: false,
        invert_media: false,
        allow_paid_floodskip: false,
        peer: peer.into(),
        reply_to: None,
        message,
        random_id,
        reply_markup: None,
        entities: (!entities.is_empty()).then_some(entities),
        schedule_date: None,
        schedule_repeat_period: None,
        send_as: None,
        quick_reply_shortcut: None,
        effect: None,
        allow_paid_stars: None,
        suggested_post: None,
        rich_message: None,
    };

    match client.invoke(&request).await {
        Ok(updates) => SendOutcome::Delivered {
            sent_msg_id: sent_message_id(&updates),
        },
        Err(e) if is_auth_error(&e) => SendOutcome::Auth(e),
        Err(InvocationError::Rpc(rpc)) if rpc.is("RANDOM_ID_DUPLICATE") => {
            // Сообщение с этим random_id у Telegram уже есть, id он не отдаёт.
            // Это доставка, а не ошибка.
            SendOutcome::Delivered { sent_msg_id: None }
        }
        Err(e) => match flood_wait(&e) {
            Some(wait) => SendOutcome::Wait(wait, e.to_string()),
            None => match &e {
                InvocationError::Rpc(rpc) if rpc.code == 400 || rpc.code == 403 => {
                    SendOutcome::Rejected(e.to_string())
                }
                _ => SendOutcome::Unclear(e.to_string()),
            },
        },
    }
}

/// Доставить все части прогона, у которых нет `delivered_at`.
///
/// Возвращает `Ok(None)`, если доставлено всё; `Ok(Some(причина))` — если
/// прогон остаётся в `publishing` и ждёт следующего триггера или владельца.
pub async fn deliver_pending(
    client: &Client,
    db: &Db,
    run: &Run,
    cancel: &Cancel,
    retry_window: Option<ChronoDuration>,
) -> Result<Option<String>> {
    let dest = run
        .destination()
        .ok_or_else(|| anyhow!("у прогона #{} не зафиксирован адресат", run.id))?;
    let peer = destination_ref(dest)?;

    db.mark_publishing(run.id).await?;
    let chunks = db.chunks(run.id).await?;
    let total = chunks.len();
    let mut first_send_at = run.first_send_at;

    for chunk in chunks.iter().filter(|c| c.delivered_at.is_none()) {
        // Токен остановки проверяем МЕЖДУ частями: сама отправка не отменяется,
        // иначе не понять, ушла часть или нет.
        if cancel.is_cancelled() {
            return Ok(Some(format!(
                "остановка сигналом: доставлено {} из {total} частей",
                chunk.idx
            )));
        }
        if chunk.idx > 0 && cancel.sleep(PAUSE_BETWEEN_CHUNKS).await.is_err() {
            return Ok(Some("остановка сигналом".into()));
        }

        let mut attempt = 0;
        loop {
            if cancel.is_cancelled() {
                return Ok(Some("остановка сигналом".into()));
            }
            if first_send_at.is_some_and(|first| retry_expired(first, retry_window, Utc::now())) {
                return Ok(Some(format!(
                    "окно автоповтора вышло: /resume {} или /abandon {}",
                    run.id, run.id
                )));
            }
            db.mark_first_send(run.id).await?;
            first_send_at.get_or_insert(Utc::now());
            attempt += 1;
            match send_chunk(client, peer, &chunk.html, chunk.random_id).await {
                SendOutcome::Delivered { sent_msg_id } => {
                    db.mark_delivered(run.id, chunk.idx, sent_msg_id).await?;
                    tracing::info!(
                        run = run.id,
                        part = chunk.idx + 1,
                        of = total,
                        msg_id = ?sent_msg_id,
                        "часть доставлена"
                    );
                    break;
                }
                SendOutcome::Auth(e) => {
                    return Err(anyhow::Error::new(e).context("сессия Telegram недействительна"))
                }
                SendOutcome::Rejected(reason) => {
                    return Ok(Some(format!(
                        "часть {}/{total} отклонена: {reason}",
                        chunk.idx + 1
                    )))
                }
                SendOutcome::Wait(wait, reason) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Ok(Some(give_up(chunk.idx, total, attempt, &reason)));
                    }
                    tracing::warn!(run = run.id, part = chunk.idx + 1, ?wait, %reason, "FLOOD_WAIT, ждём");
                    if cancel.sleep(wait).await.is_err() {
                        return Ok(Some("остановка сигналом".into()));
                    }
                }
                SendOutcome::Unclear(reason) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Ok(Some(give_up(chunk.idx, total, attempt, &reason)));
                    }
                    let delay = Duration::from_secs(2u64.pow(attempt));
                    tracing::warn!(run = run.id, part = chunk.idx + 1, ?delay, %reason, "неясный исход, повтор с тем же random_id");
                    if cancel.sleep(delay).await.is_err() {
                        return Ok(Some("остановка сигналом".into()));
                    }
                }
            }
        }
    }

    // Критерий завершения — delivered_at у всех частей, а не sent_msg_id.
    let pending = db
        .chunks(run.id)
        .await?
        .into_iter()
        .filter(|c| c.delivered_at.is_none())
        .count();

    if pending == 0 {
        Ok(None)
    } else {
        Ok(Some(format!("не доставлено частей: {pending} из {total}")))
    }
}

fn retry_expired(
    first_send: DateTime<Utc>,
    window: Option<ChronoDuration>,
    now: DateTime<Utc>,
) -> bool {
    window.is_some_and(|window| now - first_send > window)
}

/// Отправить готовые части заново, с новыми `random_id`: осознанный повтор
/// (`/last`), а не восстановление.
pub async fn resend(
    client: &Client,
    peer: PeerRef,
    chunks: &[String],
    cancel: &Cancel,
) -> Result<()> {
    for (index, html) in chunks.iter().enumerate() {
        if cancel.is_cancelled() || (index > 0 && cancel.sleep(PAUSE_BETWEEN_CHUNKS).await.is_err())
        {
            anyhow::bail!("повторная отправка остановлена сигналом");
        }
        match send_chunk(client, peer, html, rand::random()).await {
            SendOutcome::Delivered { .. } => {}
            SendOutcome::Auth(e) => {
                return Err(anyhow::Error::new(e).context("сессия Telegram недействительна"))
            }
            SendOutcome::Rejected(reason)
            | SendOutcome::Unclear(reason)
            | SendOutcome::Wait(_, reason) => {
                return Err(anyhow!(
                    "не удалось отправить часть {}/{}: {reason}",
                    index + 1,
                    chunks.len()
                ))
            }
        }
    }
    Ok(())
}

fn give_up(idx: i64, total: usize, attempt: u32, reason: &str) -> String {
    format!(
        "часть {}/{total} не отправлена после {attempt} попыток: {reason}",
        idx + 1
    )
}

/// Id отправленного сообщения, если Telegram его вернул. Нигде не участвует в
/// логике — только в `/status` для справки.
fn sent_message_id(updates: &tl::enums::Updates) -> Option<i32> {
    use tl::enums::Updates;

    match updates {
        Updates::UpdateShortSentMessage(sent) => Some(sent.id),
        Updates::UpdateShortMessage(m) => Some(m.id),
        Updates::UpdateShortChatMessage(m) => Some(m.id),
        Updates::UpdateShort(short) => new_message_id(&short.update),
        Updates::Combined(combined) => combined.updates.iter().find_map(new_message_id),
        Updates::Updates(updates) => updates.updates.iter().find_map(new_message_id),
        _ => None,
    }
}

fn new_message_id(update: &tl::enums::Update) -> Option<i32> {
    use tl::enums::Update;
    match update {
        Update::NewMessage(u) => Some(message_id(&u.message)),
        Update::NewChannelMessage(u) => Some(message_id(&u.message)),
        _ => None,
    }
}

fn message_id(message: &tl::enums::Message) -> i32 {
    use tl::enums::Message;
    match message {
        Message::Empty(m) => m.id,
        Message::Message(m) => m.id,
        Message::Service(m) => m.id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_session::types::PeerKind;

    #[test]
    fn automatic_retry_stops_when_the_window_expires() {
        let first = Utc::now();
        let window = ChronoDuration::hours(2);
        assert!(!retry_expired(first, Some(window), first + window));
        assert!(retry_expired(
            first,
            Some(window),
            first + window + ChronoDuration::seconds(1)
        ));
        assert!(!retry_expired(
            first,
            None,
            first + ChronoDuration::days(30)
        ));
    }

    #[test]
    fn saved_destination_restores_the_same_peer() {
        // Адресат зафиксирован на момент `summarized`: восстановление обязано
        // попасть в тот же чат, а не в тот, что сейчас в конфиге.
        let channel = Destination {
            peer_id: -1001234567890,
            access_hash: 42,
        };
        let peer = destination_ref(channel).unwrap();
        assert_eq!(peer.id.kind(), PeerKind::Channel);
        assert_eq!(peer.id.bare_id(), Some(1234567890));
        assert_eq!(peer.auth.hash(), 42);

        let me = Destination {
            peer_id: 777000,
            access_hash: 7,
        };
        let peer = destination_ref(me).unwrap();
        assert_eq!(peer.id.kind(), PeerKind::User);
        assert_eq!(peer.id.bare_id(), Some(777000));
    }

    #[test]
    fn a_meaningless_destination_is_rejected_instead_of_guessed() {
        let broken = Destination {
            peer_id: 0,
            access_hash: 0,
        };
        assert!(destination_ref(broken).is_err());
    }

    #[test]
    fn sent_id_is_taken_from_short_updates() {
        let updates =
            tl::enums::Updates::UpdateShortSentMessage(tl::types::UpdateShortSentMessage {
                out: true,
                id: 555,
                pts: 1,
                pts_count: 1,
                date: 0,
                media: None,
                entities: None,
                ttl_period: None,
            });
        assert_eq!(sent_message_id(&updates), Some(555));
    }

    #[test]
    fn missing_id_is_not_an_error_for_the_journal() {
        // Для части пиров Telegram id не возвращает: критерий доставки —
        // delivered_at, а не sent_msg_id.
        let updates = tl::enums::Updates::TooLong;
        assert_eq!(sent_message_id(&updates), None);
    }
}
