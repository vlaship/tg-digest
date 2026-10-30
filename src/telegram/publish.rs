//! Sending the digest and the delivery journal.
//!
//! Telegram itself gives idempotency: `messages.sendMessage` accepts
//! `random_id` "to prevent sending the same message twice". `random_id` is issued
//! once when a part is written to the journal and is reused in all retries,
//! including retries after a container restart. The length of the server dedup window
//! is not documented, so this lowers the risk but is not a guarantee: outside
//! `publish_retry_window_hours` only the owner retries (`/resume`).
//!
//! The high-level `Client::send_message` does not accept `random_id` — we send through
//! `invoke(messages::SendMessage { … })` and take the markup from the same HTML parser.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use grammers_client::client::Client;
use grammers_client::parsers::parse_html_message;
use grammers_client::{tl, InvocationError};
use grammers_session::types::{PeerAuth, PeerId, PeerRef};
use std::time::Duration;

use crate::cancel::Cancel;
use crate::db::{Chunk, Db, Destination, Run};
use crate::telegram::bot::Bot;
use crate::telegram::client::{flood_wait, is_auth_error};

/// Pause between parts of a long digest, so Telegram does not see it as flooding.
const PAUSE_BETWEEN_CHUNKS: Duration = Duration::from_millis(700);
/// How many times we retry a part with an unclear result before we leave the run
/// in `publishing` status and call the owner.
const MAX_ATTEMPTS: u32 = 4;

/// The result of one send attempt. The split into "unclear" and "refused" is the key:
/// an unclear result is retried with the same `random_id`, a refusal is useless to retry.
#[derive(Debug)]
pub enum SendOutcome {
    /// Telegram has the part. `sent_msg_id` can be missing — for some
    /// peers the answer has no id, and that is normal.
    Delivered { sent_msg_id: Option<i32> },
    /// Timeout, broken connection, error without a code: we save nothing, we retry.
    Unclear(String),
    /// FLOOD_WAIT: how long they ask us to wait.
    Wait(Duration, String),
    /// `CHAT_WRITE_FORBIDDEN`, `PEER_ID_INVALID` and others that a retry does not fix.
    Rejected(String),
    /// The session is not valid — this is not a run error, but the end of the daemon's work.
    Auth(InvocationError),
}

/// Where the bot posts: `"bot"` is the chat with the owner (`"me"` means the same, for
/// old configs), otherwise `@channel`, where the bot must be an admin.
///
/// Resolved once, when the parts are written to the journal: changing `destination` in
/// the config and restarting does not send the rest of an undelivered digest to another chat.
pub async fn resolve_destination(bot: &Bot, destination: &str) -> Result<Destination> {
    let peer = if matches!(destination, "bot" | "me") {
        bot.owner_ref().await?
    } else {
        let username = destination.trim_start_matches('@');
        bot.client
            .resolve_username(username)
            .await
            .with_context(|| format!("could not resolve {destination}"))?
            .ok_or_else(|| anyhow!("channel {destination} not found"))?
            .to_ref()
            .await
            .map_err(|e| anyhow!("could not get a reference to {destination}: {e}"))?
            .ok_or_else(|| anyhow!("channel {destination} is not available for sending"))?
    };

    Ok(Destination {
        peer_id: peer
            .id
            .bot_api_dialog_id()
            .ok_or_else(|| anyhow!("destination {destination} has no id"))?,
        access_hash: peer.auth.hash(),
    })
}

/// Everybody who gets the digest. The first one is the main destination of the
/// delivery journal, the rest get copies.
///
/// For a channel this is only the channel: it already reaches everyone who follows it.
/// For the chat with the bot: the owner, then the other allowed users — except those
/// who sent `/stop`. The list may be empty: then nobody wants the digest.
///
/// The owner who did not write to the bot yet is an error, as before. Another user
/// who never wrote to the bot cannot get messages from it, so they are skipped with a
/// warning instead of failing the run.
pub async fn recipients(bot: &Bot, db: &Db, destination: &str) -> Result<Vec<Destination>> {
    if !matches!(destination, "bot" | "me") {
        return Ok(vec![resolve_destination(bot, destination).await?]);
    }

    let muted = db.muted_users().await?;
    let is_muted = |user: grammers_session::types::PeerId| {
        user.bare_id().is_some_and(|id| muted.contains(&id))
    };

    let mut out = Vec::new();
    if !is_muted(bot.owner) {
        out.push(to_destination(bot.owner_ref().await?));
    }
    for user in bot.others().filter(|&u| !is_muted(u)) {
        match bot.user_ref(user).await {
            Ok(peer) => out.push(to_destination(peer)),
            Err(e) => tracing::warn!(
                user = ?user.bare_id(),
                error = %format!("{e:#}"),
                "allowed user does not get the digest"
            ),
        }
    }
    Ok(out)
}

fn to_destination(peer: PeerRef) -> Destination {
    Destination {
        peer_id: peer.id.bot_api_dialog_id_unchecked(),
        access_hash: peer.auth.hash(),
    }
}

/// The `random_id` of a copy for another recipient: different for each recipient, but
/// the same on every retry, so Telegram still drops duplicates.
fn copy_random_id(random_id: i64, peer_id: i64) -> i64 {
    random_id ^ peer_id.wrapping_mul(0x9E37_79B9_7F4A_7C15_u64 as i64)
}

pub fn destination_ref(dest: Destination) -> Result<PeerRef> {
    let id = PeerId::from_bot_api_dialog_id(dest.peer_id)
        .ok_or_else(|| anyhow!("{} does not look like a chat id", dest.peer_id))?;
    Ok(PeerRef {
        id,
        auth: PeerAuth::from_hash(dest.access_hash),
    })
}

/// One attempt to send a part.
pub async fn send_chunk(client: &Client, peer: PeerRef, html: &str, random_id: i64) -> SendOutcome {
    let (message, entities) = parse_html_message(html);

    let request = tl::functions::messages::SendMessage {
        // link_preview is off: otherwise Telegram stretches the preview of the first
        // source link over the whole post and the digest layout breaks.
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
            // Telegram already has a message with this random_id, it does not give the id.
            // This is a delivery, not an error.
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

/// Deliver all parts of the run that have no `delivered_at`.
///
/// Returns `Ok(None)` if everything is delivered; `Ok(Some(reason))` if the
/// run stays in `publishing` and waits for the next trigger or for the owner.
pub async fn deliver_pending(
    client: &Client,
    db: &Db,
    run: &Run,
    cancel: &Cancel,
    retry_window: Option<ChronoDuration>,
) -> Result<Option<String>> {
    let dest = run
        .destination()
        .ok_or_else(|| anyhow!("run #{} has no fixed destination", run.id))?;
    let peer = destination_ref(dest)?;
    let extras = run
        .extra_dests
        .iter()
        .map(|d| destination_ref(*d))
        .collect::<Result<Vec<_>>>()?;

    db.mark_publishing(run.id).await?;
    let chunks = db.chunks(run.id).await?;
    let total = chunks.len();
    let mut send = Sender {
        client,
        db,
        run,
        cancel,
        retry_window,
        first_send_at: run.first_send_at,
        total,
    };

    for chunk in chunks.iter().filter(|c| c.delivered_at.is_none()) {
        // We check the shutdown token BETWEEN parts: sending itself is not cancelled,
        // or we cannot know whether the part went out or not.
        if cancel.is_cancelled() {
            return Ok(Some(format!(
                "stopped by signal: delivered {} of {total} parts",
                chunk.idx
            )));
        }
        if chunk.idx > 0 && cancel.sleep(PAUSE_BETWEEN_CHUNKS).await.is_err() {
            return Ok(Some("stopped by signal".into()));
        }

        // The main destination decides the delivery: if it fails, the run waits.
        let sent_msg_id = match send.part(peer, chunk, chunk.random_id).await? {
            Attempt::Delivered(id) => id,
            Attempt::Stop(reason) => return Ok(Some(reason)),
        };

        // Copies for the other users. A failure here does not hold the digest for the
        // owner: it is only logged. On a retry the same `random_id` keeps them unique.
        for extra in &extras {
            let random_id = copy_random_id(chunk.random_id, extra.id.bot_api_dialog_id_unchecked());
            if let Attempt::Stop(reason) = send.part(*extra, chunk, random_id).await? {
                tracing::warn!(
                    run = run.id,
                    part = chunk.idx + 1,
                    user = ?extra.id.bare_id(),
                    %reason,
                    "copy for an allowed user not delivered"
                );
                if cancel.is_cancelled() {
                    return Ok(Some("stopped by signal".into()));
                }
            }
        }

        db.mark_delivered(run.id, chunk.idx, sent_msg_id).await?;
        tracing::info!(
            run = run.id,
            part = chunk.idx + 1,
            of = total,
            msg_id = ?sent_msg_id,
            copies = extras.len(),
            "part delivered"
        );
    }

    // The completion rule is delivered_at for all parts, not sent_msg_id.
    let pending = db
        .chunks(run.id)
        .await?
        .into_iter()
        .filter(|c| c.delivered_at.is_none())
        .count();

    if pending == 0 {
        Ok(None)
    } else {
        Ok(Some(format!("parts not delivered: {pending} of {total}")))
    }
}

/// How sending one part to one recipient ended.
enum Attempt {
    Delivered(Option<i32>),
    /// Stop here: the reason goes to the owner, the run stays in `publishing`.
    Stop(String),
}

/// What all sends of one delivery share: the retry rules and the time of the
/// first attempt, from which the retry window is counted.
struct Sender<'a> {
    client: &'a Client,
    db: &'a Db,
    run: &'a Run,
    cancel: &'a Cancel,
    retry_window: Option<ChronoDuration>,
    first_send_at: Option<DateTime<Utc>>,
    total: usize,
}

impl Sender<'_> {
    /// Send one part to one recipient, retrying an unclear result with the same
    /// `random_id`. Only a dead session is an error: then the daemon must exit.
    async fn part(&mut self, peer: PeerRef, chunk: &Chunk, random_id: i64) -> Result<Attempt> {
        let (run, total) = (self.run, self.total);
        let mut attempt = 0;
        loop {
            if self.cancel.is_cancelled() {
                return Ok(Attempt::Stop("stopped by signal".into()));
            }
            if self
                .first_send_at
                .is_some_and(|first| retry_expired(first, self.retry_window, Utc::now()))
            {
                return Ok(Attempt::Stop(format!(
                    "auto-retry window is over: /resume {} or /abandon {}",
                    run.id, run.id
                )));
            }
            self.db.mark_first_send(run.id).await?;
            self.first_send_at.get_or_insert(Utc::now());
            attempt += 1;
            match send_chunk(self.client, peer, &chunk.html, random_id).await {
                SendOutcome::Delivered { sent_msg_id } => {
                    return Ok(Attempt::Delivered(sent_msg_id))
                }
                SendOutcome::Auth(e) => {
                    return Err(anyhow::Error::new(e).context("Telegram session is not valid"))
                }
                SendOutcome::Rejected(reason) => {
                    return Ok(Attempt::Stop(format!(
                        "part {}/{total} refused: {reason}",
                        chunk.idx + 1
                    )))
                }
                SendOutcome::Wait(wait, reason) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Ok(Attempt::Stop(give_up(chunk.idx, total, attempt, &reason)));
                    }
                    tracing::warn!(run = run.id, part = chunk.idx + 1, ?wait, %reason, "FLOOD_WAIT, waiting");
                    if self.cancel.sleep(wait).await.is_err() {
                        return Ok(Attempt::Stop("stopped by signal".into()));
                    }
                }
                SendOutcome::Unclear(reason) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Ok(Attempt::Stop(give_up(chunk.idx, total, attempt, &reason)));
                    }
                    let delay = Duration::from_secs(2u64.pow(attempt));
                    tracing::warn!(run = run.id, part = chunk.idx + 1, ?delay, %reason, "unclear result, retry with the same random_id");
                    if self.cancel.sleep(delay).await.is_err() {
                        return Ok(Attempt::Stop("stopped by signal".into()));
                    }
                }
            }
        }
    }
}

fn retry_expired(
    first_send: DateTime<Utc>,
    window: Option<ChronoDuration>,
    now: DateTime<Utc>,
) -> bool {
    window.is_some_and(|window| now - first_send > window)
}

/// Send ready parts again with new `random_id`: a deliberate repeat
/// (`/last`), not a recovery.
pub async fn resend(
    client: &Client,
    peer: PeerRef,
    chunks: &[String],
    cancel: &Cancel,
) -> Result<()> {
    for (index, html) in chunks.iter().enumerate() {
        if cancel.is_cancelled() || (index > 0 && cancel.sleep(PAUSE_BETWEEN_CHUNKS).await.is_err())
        {
            anyhow::bail!("resending stopped by signal");
        }
        match send_chunk(client, peer, html, rand::random()).await {
            SendOutcome::Delivered { .. } => {}
            SendOutcome::Auth(e) => {
                return Err(anyhow::Error::new(e).context("Telegram session is not valid"))
            }
            SendOutcome::Rejected(reason)
            | SendOutcome::Unclear(reason)
            | SendOutcome::Wait(_, reason) => {
                return Err(anyhow!(
                    "could not send part {}/{}: {reason}",
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
        "part {}/{total} not sent after {attempt} attempts: {reason}",
        idx + 1
    )
}

/// Id of the sent message, if Telegram returned it. It takes no part in the
/// logic — only in `/status` for information.
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
