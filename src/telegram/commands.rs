//! Controlling the service with commands to the bot.
//!
//! The bot listens to updates and reacts **only** to the owner (the personal
//! account from `[telegram]`). Anybody can write to a bot, so everybody else is
//! ignored.
//!
//! Channels are named by `@username` or by numeric `id`: private channels have
//! no username, so `/list` and the `/add` reply always show the `id`.

use anyhow::{bail, Context, Result};
use grammers_client::client::UpdatesConfiguration;
use grammers_client::message::InputMessage;
use grammers_client::peer::Peer;
use grammers_client::update::Update;
use grammers_session::updates::UpdatesLike;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinSet;

use crate::cancel::Cancel;
use crate::db::Channel;
use crate::run::{Runner, TRIGGER_MANUAL};
use crate::telegram::client::is_auth_error;
use crate::telegram::{channels, publish};

const HELP: &str = "\
Commands:
/add @channel | link | id … — add one or more channels
/rm @channel | id — remove a channel
/list — channels, cursors, errors
/reset id — read the channel again from the lookback_hours window
/skip — move all channels to their newest message: older posts are skipped
/run — run now (or deliver an undelivered digest)
/status — version and last run: status, posts, tokens
/model [provider/model] — list the models, or switch
/fallback — switch to the next fallback model and run now
/last — send the last digest again
/resume N — deliver an old run (a duplicate is possible)
/abandon N — cancel an undelivered run
/help — this help";

/// Run the updates loop until the shutdown signal comes.
pub async fn listen(
    runner: Arc<Runner>,
    updates: UnboundedReceiver<UpdatesLike>,
    cancel: Cancel,
) -> Result<()> {
    let bot = runner.bot()?;

    let mut stream = bot
        .client
        .stream_updates(
            updates,
            UpdatesConfiguration {
                catch_up: false,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("could not subscribe to updates: {e}"))?;

    tracing::info!("listening for commands to the bot, /help for the list");
    let mut commands = JoinSet::<Result<()>>::new();

    loop {
        let update = tokio::select! {
            _ = cancel.cancelled() => {
                // Let an in-flight send record its delivery before exiting.
                while let Some(result) = commands.join_next().await {
                    result.context("command handler failed")??;
                }
                tracing::info!("command listener stopped");
                return Ok(());
            }
            result = commands.join_next(), if !commands.is_empty() => {
                result.expect("there is a handler").context("command handler failed")??;
                continue;
            }
            update = stream.next() => update,
        };

        let update = match update {
            Ok(update) => update,
            // A revoked session breaks the loop: this is fatal, not "an error
            // of one update".
            Err(e) if is_auth_error(&e) => {
                return Err(anyhow::Error::new(e).context("Telegram session is not valid"))
            }
            Err(e) => {
                tracing::warn!(error = %e, "error reading updates");
                continue;
            }
        };

        let Update::NewMessage(message) = update else {
            tracing::debug!("update skipped: not a new message");
            continue;
        };
        // The bot's own replies are outgoing, so they never come back as commands.
        if message.outgoing() {
            continue;
        }
        // In a private chat with the bot the peer is the user who wrote.
        let user = message.peer_id();
        if user != bot.owner {
            tracing::info!(user = ?user.bare_id(), "message ignored: user is not the owner");
            continue;
        }
        let text = message.text().trim().to_string();
        if !text.starts_with('/') {
            continue;
        }

        tracing::info!(command = %text, "command from the owner");
        let runner = runner.clone();
        let cancel = cancel.clone();
        commands.spawn(async move {
            // A long command says at once that it has started: otherwise the chat is
            // silent for minutes and it looks like nothing happened.
            if is_slow(&text) {
                let note = InputMessage::new()
                    .text("⏳ working on it, this can take a few minutes…")
                    .link_preview(false);
                if let Err(e) = message.respond(note).await {
                    tracing::warn!(error = %e, "could not send the working note");
                }
            }
            let reply = match handle(&runner, &text).await {
                Ok(reply) => reply,
                Err(e) if crate::telegram::client::is_fatal(&e) => return Err(e),
                // We show a command error to the owner, we do not hide it in the log.
                Err(e) => format!("failed: {e:#}"),
            };

            // Telegram does not cut a long reply (`/list` with fifty channels),
            // it rejects it completely — so we split it ourselves.
            for piece in crate::digest::split_plain(&reply) {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                if let Err(e) = message
                    .respond(InputMessage::new().text(piece).link_preview(false))
                    .await
                {
                    if is_auth_error(&e) {
                        return Err(anyhow::Error::new(e)
                            .context("Telegram session is not valid; run login"));
                    }
                    tracing::warn!(error = %e, "could not reply to the command");
                    break;
                }
            }
            Ok(())
        });
    }
}

async fn handle(runner: &Arc<Runner>, text: &str) -> Result<String> {
    let mut parts = text.splitn(2, char::is_whitespace);
    // Telegram may add the bot name: `/list@my_digest_bot`.
    let command = parts
        .next()
        .unwrap_or_default()
        .split('@')
        .next()
        .unwrap_or_default();
    let arg = parts.next().unwrap_or("").trim();

    match command {
        "/help" => Ok(HELP.into()),
        "/start" => Ok(HELP.into()),
        "/model" => model(runner, arg).await,
        "/fallback" => fallback(runner).await,
        "/list" => list(runner).await,
        "/status" => status(runner).await,
        "/run" => Ok(runner.trigger(TRIGGER_MANUAL).await?.to_string()),
        "/resume" => match arg.parse::<i64>() {
            Ok(id) => Ok(runner.resume(id).await?.to_string()),
            Err(_) => Ok("a run number is needed: /resume 12".into()),
        },
        "/abandon" => match arg.parse::<i64>() {
            Ok(id) => Ok(runner.abandon(id).await?.to_string()),
            Err(_) => Ok("a run number is needed: /abandon 12".into()),
        },
        // Futures are lazy: the action does not start until `guarded` lets it through.
        "/add" => guarded(runner, add_many(runner, arg)).await,
        "/rm" => guarded(runner, remove(runner, arg)).await,
        "/reset" => guarded(runner, reset(runner, arg)).await,
        "/skip" => guarded(runner, skip(runner)).await,
        "/last" => guarded(runner, last(runner)).await,
        other => Ok(format!("unknown command {other}\n\n{HELP}")),
    }
}

/// Commands that touch cursors go under the same lock as the run,
/// and they are refused if there is an undelivered run: its `runs.cursors` would
/// overwrite any reset on delivery.
async fn guarded<Fut>(runner: &Arc<Runner>, action: Fut) -> Result<String>
where
    Fut: std::future::Future<Output = Result<String>>,
{
    let Some(_guard) = runner.try_lock() else {
        let id = runner
            .db
            .last_run()
            .await?
            .map(|r| r.id)
            .unwrap_or_default();
        return Ok(format!("run #{id} is going, try again later"));
    };
    if let Some(run) = runner.db.unfinished_run().await? {
        return Ok(format!(
            "there is an undelivered run #{} — /run to deliver it or /abandon {}",
            run.id, run.id
        ));
    }
    action.await
}

/// `/model` lists the models from the config, `/model provider/model` switches.
async fn model(runner: &Arc<Runner>, arg: &str) -> Result<String> {
    let current = runner.model().await;
    if arg.is_empty() {
        let cfg = &runner.cfg;
        let mut out = String::from("Models:");
        for key in cfg.model_keys() {
            let mark = if key == current { "✅" } else { "•" };
            out.push_str(&format!("\n{mark} {key}"));
            if key == current {
                out.push_str(" (now)");
            }
            if let Some(place) = cfg.fallback.iter().position(|m| *m == key) {
                out.push_str(&format!(" (fallback {})", place + 1));
            }
            if let Err(e) = cfg.model(&key) {
                out.push_str(&format!(" — {e:#}"));
            }
        }
        out.push_str("\n\n/model provider/model — switch");
        return Ok(out);
    }
    match runner.set_model(arg).await {
        Ok(()) => Ok(format!("model: {arg} (was {current})")),
        Err(e) => Ok(format!("cannot switch to {arg}: {e:#}")),
    }
}

/// The owner agrees to the fallback model after an error notice: switch to the
/// next model in the `fallback` list and build the digest at once.
async fn fallback(runner: &Arc<Runner>) -> Result<String> {
    let current = runner.model().await;
    let Some(fallback) = runner.next_fallback().await else {
        return Ok("there is no other model in fallback in the config".into());
    };
    if let Err(e) = runner.set_model(&fallback).await {
        return Ok(format!("cannot switch to {fallback}: {e:#}"));
    }
    let outcome = runner.trigger(TRIGGER_MANUAL).await?;
    Ok(format!(
        "model: {fallback} (was {current}). /model {current} switches back.\n\n{outcome}"
    ))
}

async fn list(runner: &Arc<Runner>) -> Result<String> {
    let channels = runner.db.channels().await?;
    if channels.is_empty() {
        return Ok("no channels yet: add one with /add @channel".into());
    }

    let mut out = String::new();
    for channel in &channels {
        out.push_str(&format!(
            "{} {} — id {}, {}, cursor {}{}\n",
            if channel.enabled { "✅" } else { "⛔" },
            channel.title,
            channel.id,
            channel
                .username
                .as_ref()
                .map(|u| format!("@{u}"))
                .unwrap_or_else(|| "private".into()),
            channel.last_msg_id,
            channel
                .last_error
                .as_ref()
                .map(|e| format!("\n   error: {e}"))
                .unwrap_or_default(),
        ));
    }
    Ok(out)
}

async fn status(runner: &Arc<Runner>) -> Result<String> {
    let version = crate::version();
    let Some(run) = runner.db.last_run().await? else {
        return Ok(format!("version {version}\nthere were no runs yet"));
    };

    let chunks = runner.db.chunks(run.id).await?;
    let delivered = chunks.iter().filter(|c| c.delivered_at.is_some()).count();

    let mut out = format!(
        "version {version}\nrun #{} ({}): {}\nstarted {}\nfinished {}\nposts {}, topics {}\ntokens: {} / {}\nparts: {delivered} of {}\ndestination: {}",
        run.id,
        run.trigger,
        run.status,
        run.started_at.format("%Y-%m-%d %H:%M UTC"),
        run.finished_at
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "—".into()),
        run.posts.unwrap_or(0),
        run.topics.unwrap_or(0),
        run.input_tokens.unwrap_or(0),
        run.output_tokens.unwrap_or(0),
        chunks.len(),
        run.dest_label.as_deref().unwrap_or("—"),
    );
    if let Some(error) = &run.error {
        out.push_str(&format!("\nerror: {error}"));
    }

    // A part with `delivered_at` but without `sent_msg_id` is a delivery for which
    // Telegram did not return an id (or answered RANDOM_ID_DUPLICATE). We say so.
    for chunk in &chunks {
        out.push_str(&match (&chunk.delivered_at, chunk.sent_msg_id) {
            (Some(at), Some(id)) => format!(
                "\n  part {}: delivered {} (msg {id})",
                chunk.idx + 1,
                at.format("%H:%M")
            ),
            (Some(at), None) => format!(
                "\n  part {}: delivered {} (no message id came)",
                chunk.idx + 1,
                at.format("%H:%M")
            ),
            (None, _) => format!("\n  part {}: not delivered", chunk.idx + 1),
        });
    }

    let channels = runner.db.channels().await?;
    let broken: Vec<&Channel> = channels.iter().filter(|c| c.last_error.is_some()).collect();
    if !broken.is_empty() {
        out.push_str("\n\nchannels with errors:");
        for channel in broken {
            out.push_str(&format!(
                "\n{} — {}",
                channel.label(),
                channel.last_error.as_deref().unwrap_or_default()
            ));
        }
    }
    Ok(out)
}

/// The channel list lives only in the database, so `/add` writes there directly.
/// A channel that was removed with `/rm` comes back with a fresh cursor.
/// `/add` with several channels separated by spaces, commas or new lines. Each one
/// is added on its own: an error in one channel does not stop the others.
async fn add_many(runner: &Arc<Runner>, arg: &str) -> Result<String> {
    let items: Vec<&str> = arg
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .collect();
    if items.len() <= 1 {
        return add(runner, arg.trim()).await;
    }

    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(match add(runner, item).await {
            Ok(reply) => reply,
            Err(e) => format!("{item}: error: {e:#}"),
        });
    }
    Ok(out.join("\n\n"))
}

async fn add(runner: &Arc<Runner>, arg: &str) -> Result<String> {
    if arg.is_empty() {
        return Ok("a channel is needed: /add @name, /add https://t.me/+invite or /add id".into());
    }

    // Already in the list: say so and touch nothing, its cursor stays. For a username
    // or an id the database answers without a request to Telegram.
    let invite = arg.contains("t.me/+") || arg.contains("joinchat");
    if !invite {
        if let Some(channel) = runner.db.find_channel(username_from(arg)).await? {
            if channel.enabled {
                return Ok(already_added(&channel));
            }
        }
    }

    let peer = resolve_new_channel(runner, arg).await?;
    // An invite link is resolved only now, so check again by id.
    if let Some(channel) = runner.db.channel(peer.id).await? {
        if channel.enabled {
            return Ok(already_added(&channel));
        }
    }

    let (username, title) = channels::describe(&runner.client, peer).await?;
    runner
        .db
        .upsert_channel(peer.id, peer.access_hash, username.as_deref(), &title)
        .await?;

    // The account follows the channel: start from its read mark, so the posts it did
    // not read yet go into the digest.
    match channels::read_mark(&runner.client, peer).await {
        Ok(Some(mark)) => {
            runner.db.set_cursor(peer.id, mark).await?;
            return Ok(format!(
                "added: {title}, id {}. The digest starts from your read mark in Telegram \
                 (cursor {mark}): the posts you did not read go into it",
                peer.id
            ));
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), channel = peer.id, "could not read the read mark, the newest post is used")
        }
    }

    // Otherwise start from the newest message: only posts after adding go into the digest.
    // If we cannot read it, the channel keeps cursor 0 (the lookback_hours window).
    match channels::last_message_id(&runner.client, peer).await {
        Ok(Some(last)) => runner.db.set_cursor(peer.id, last).await?,
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), channel = peer.id, "could not read the last message, the lookback window is used")
        }
    }

    Ok(format!(
        "added: {title}, id {}. Only new posts from now on go into the digest \
         (/reset {} reads the last lookback_hours too)",
        peer.id, peer.id
    ))
}

fn already_added(channel: &Channel) -> String {
    format!(
        "already in the list: {}, id {}. /list shows all channels",
        channel.title, channel.id
    )
}

async fn remove(runner: &Arc<Runner>, arg: &str) -> Result<String> {
    if arg.is_empty() {
        return Ok("a channel is needed: /rm @name or /rm id".into());
    }
    let Some(channel) = runner.db.find_channel(arg).await? else {
        return Ok(format!(
            "channel {arg} is not in the database, /list shows the list"
        ));
    };

    // Disabled, not deleted: `/list` still shows the channel with its last error.
    runner.db.disable_channel(channel.id).await?;

    Ok(format!("disabled: {}, id {}", channel.title, channel.id))
}

/// `/skip`: move every enabled channel to its newest message, so the next digest
/// has only posts that come after this. A channel that cannot be read keeps its cursor.
async fn skip(runner: &Arc<Runner>) -> Result<String> {
    let channels = runner.db.enabled_channels().await?;
    if channels.is_empty() {
        return Ok("no channels yet: add one with /add @channel".into());
    }

    let (mut moved, mut failed) = (0, Vec::new());
    for channel in &channels {
        let peer = channels::ChannelPeer {
            id: channel.id,
            access_hash: channel.access_hash,
        };
        match channels::last_message_id(&runner.client, peer).await {
            Ok(Some(last)) => {
                runner.db.set_cursor(channel.id, last).await?;
                moved += 1;
            }
            Ok(None) => {}
            Err(e) if crate::telegram::client::is_fatal(&e) => return Err(e),
            Err(e) => failed.push(format!("{}: {e:#}", channel.label())),
        }
    }

    let mut out = format!(
        "moved {moved} of {} channels to their newest message: older posts are skipped",
        channels.len()
    );
    if !failed.is_empty() {
        out.push_str(&format!("\nnot moved:\n{}", failed.join("\n")));
    }
    Ok(out)
}

async fn reset(runner: &Arc<Runner>, arg: &str) -> Result<String> {
    let Some(channel) = runner.db.find_channel(arg).await? else {
        return Ok(format!(
            "channel {arg} is not in the database, /list shows the list"
        ));
    };
    runner.db.reset_channel(channel.id).await?;
    Ok(format!(
        "{}: cursor reset, the next run reads a window of {} h",
        channel.title, runner.cfg.telegram.lookback_hours
    ))
}

/// A deliberate repeat, not a recovery: the parts go to the **current**
/// `destination` and with new `random_id`.
async fn last(runner: &Arc<Runner>) -> Result<String> {
    let Some(run) = runner.db.last_ok_run().await? else {
        return Ok("there were no published digests yet".into());
    };
    let chunks: Vec<String> = runner
        .db
        .chunks(run.id)
        .await?
        .into_iter()
        .map(|c| c.html)
        // Photo-only parts: their files are deleted after delivery, /last sends the text.
        .filter(|html| !html.is_empty())
        .collect();
    if chunks.is_empty() {
        return Ok(format!("run #{} has no saved parts", run.id));
    }

    let bot = runner.bot()?;
    let dest = publish::resolve_destination(bot, &runner.cfg.telegram.destination).await?;
    let peer = publish::destination_ref(dest)?;
    publish::resend(&bot.client, peer, &chunks, runner.cancellation()).await?;

    Ok(format!(
        "run #{} sent again to {} ({} parts)",
        run.id,
        runner.cfg.telegram.destination,
        chunks.len()
    ))
}

/// `@username`, an invite link or a numeric id.
async fn resolve_new_channel(runner: &Arc<Runner>, arg: &str) -> Result<channels::ChannelPeer> {
    let client = &runner.client;

    // A numeric id is a special case: there is nowhere to take access_hash from, and
    // without it Telegram does not answer by id. We look in the database, then in the dialogs.
    if let Ok(id) = arg.parse::<i64>() {
        let id = crate::db::bare_channel_id(id);
        let known = runner
            .db
            .channel(id)
            .await?
            .map(|channel| channels::ChannelPeer {
                id: channel.id,
                access_hash: channel.access_hash,
            });
        return match known {
            Some(peer) => Ok(peer),
            None => channels::find_in_dialogs(client, id)
                .await?
                .with_context(|| {
                    format!(
                        "channel id {id} is not among the dialogs — does the account follow it?"
                    )
                }),
        };
    }

    let peer = if arg.contains("t.me/+") || arg.contains("joinchat") {
        client
            .accept_invite_link(arg)
            .await
            .with_context(|| format!("could not join by the link {arg}"))?
            .context("the link did not return a chat")?
    } else {
        client
            .resolve_username(username_from(arg))
            .await
            .with_context(|| format!("could not resolve {arg}"))?
            .with_context(|| format!("channel {arg} not found"))?
    };

    // A username or an invite can give anything: a user, a bot, a group.
    // The digest can read only channels, and `post_url` is also made for them.
    if !matches!(peer, Peer::Channel(_)) {
        bail!("{arg} is a {}, not a channel", peer_kind(&peer));
    }

    let reference = peer
        .to_ref()
        .await
        .map_err(|e| anyhow::anyhow!("channel is not available: {e}"))?
        .context("channel is not available")?;

    Ok(channels::ChannelPeer {
        id: reference.id.bare_id().context("the channel has no id")?,
        access_hash: reference.auth.hash(),
    })
}

/// Commands that can take a long time: they read channels, call the model or send a
/// digest. The bot answers them with a "working on it" note first.
fn is_slow(text: &str) -> bool {
    let command = text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .split('@')
        .next()
        .unwrap_or_default();
    matches!(
        command,
        "/run" | "/resume" | "/last" | "/skip" | "/add" | "/fallback"
    )
}

/// The username from `@name`, `name`, `t.me/name` or `https://t.me/name/123`.
fn username_from(arg: &str) -> &str {
    let rest = arg
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let rest = rest
        .strip_prefix("t.me/")
        .or_else(|| rest.strip_prefix("telegram.me/"))
        .unwrap_or(rest);
    let name = rest.split(['/', '?']).next().unwrap_or(rest);
    name.trim_start_matches('@')
}

fn peer_kind(peer: &Peer) -> &'static str {
    match peer {
        Peer::User(user) if user.is_bot() => "bot",
        Peer::User(_) => "user",
        Peer::Group(_) => "group",
        Peer::Channel(_) => "channel",
    }
}
