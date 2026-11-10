//! Controlling the service with commands to the bot.
//!
//! The bot listens to updates and reacts **only** to the owner (the personal
//! account from `[telegram]`). Anybody can write to a bot, so everybody else is
//! ignored.
//!
//! Channels are named by `@username` or by numeric `id`: private channels have
//! no username, so `/list` and the `/add` reply always show the `id`.
//!
//! A command can start its argument with a theme tag (`/add dev @channel`,
//! `/run dev`). Without a tag `/add` uses the first theme, `/rm` and `/reset` find
//! the theme of the channel, and the other commands work on all themes.

use anyhow::{anyhow, bail, Context, Result};
use grammers_client::client::UpdatesConfiguration;
use grammers_client::message::InputMessage;
use grammers_client::peer::Peer;
use grammers_client::tl;
use grammers_client::update::Update;
use grammers_session::updates::UpdatesLike;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinSet;

use crate::cancel::Cancel;
use crate::config::Theme;
use crate::db::{Channel, Run};
use crate::run::{Outcome, Runner, TRIGGER_MANUAL};
use crate::telegram::client::is_auth_error;
use crate::telegram::{channels, publish};

/// A command reply is plain text; text between two backticks becomes `<code>`, so
/// Telegram shows it in monospace and copies it with one tap (model ids, commands).
/// Everything else is escaped. A backtick without a pair stays as it is. Pairs are
/// looked for inside one line only: a stray backtick in two channel titles must not
/// turn the lines between them into one code block.
fn reply_html(text: &str) -> String {
    text.split('\n')
        .map(line_html)
        .collect::<Vec<_>>()
        .join("\n")
}

fn line_html(text: &str) -> String {
    let parts: Vec<&str> = text.split('`').collect();
    let mut out = String::with_capacity(text.len() + 16);
    for (i, part) in parts.iter().enumerate() {
        let escaped = crate::digest::escape(part);
        let paired = i % 2 == 1 && i + 1 < parts.len();
        if paired {
            out.push_str(&format!("<code>{escaped}</code>"));
        } else {
            if i % 2 == 1 {
                out.push('`');
            }
            out.push_str(&escaped);
        }
    }
    out
}

/// Put into a command reply where a new Telegram message must start. It is a
/// control character (record separator), so it never comes from channel titles.
const MESSAGE_BREAK: char = '\u{1e}';

const HELP: &str = "\
Commands:
/add [theme] @channel | link | id … — add one or more channels (no theme: the first one)
/rm [theme] @channel | id … — remove channels (several at once, or one /rm per line)
/list — themes, channels, cursors, errors
/reset [theme] id — read the channel again from the lookback_hours window
/skip [theme] — move the channels to their newest message: older posts are skipped
/run [theme] — run now (or deliver an undelivered digest)
/reread N [theme] — read the channels again for the last N days (1–7) and run now
/status — version and the last run of each theme: status, posts, tokens
/model [provider/model] — the models from the config, or switch to any model
/models provider [word] — the models the provider has (word: a part of the id, or free)
/fallback — switch to the next fallback model and run now
/last [theme] — send the last digest again
/resume N — deliver an old run (a duplicate is possible)
/abandon N — cancel an undelivered run
/help — this help

A theme is its tag from the config, one word. A channel can be in several themes:
each theme reads it on its own.";

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
        let client = bot.client.clone();
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
            // While a long command works, the chat shows "typing…", so it is clear
            // that the bot is busy with the digest.
            let peer = if is_slow(&text) {
                message.peer_ref().await.ok().flatten()
            } else {
                None
            };
            let work = Box::pin(handle(&runner, &text));
            let result = match peer {
                Some(peer) => {
                    let (result, typing) = client
                        .action(peer)
                        .repeat(|| tl::types::SendMessageTypingAction {}, work)
                        .await;
                    if let Err(e) = typing {
                        tracing::debug!(error = %e, "could not show typing");
                    }
                    result
                }
                None => work.await,
            };
            let reply = match result {
                Ok(reply) => reply,
                Err(e) if crate::telegram::client::is_fatal(&e) => return Err(e),
                // We show a command error to the owner, we do not hide it in the log.
                Err(e) => format!("failed: {e:#}"),
            };

            // A reply can ask for several messages (`/list`: one per theme). Telegram
            // does not cut a long message either, it rejects it completely — so a
            // long part is split too.
            let pieces = reply
                .split(MESSAGE_BREAK)
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .flat_map(crate::digest::split_plain);
            for piece in pieces {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                if let Err(e) = message
                    .respond(
                        InputMessage::new()
                            .html(reply_html(&piece))
                            .link_preview(false),
                    )
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
    let arg = text
        .split_once(char::is_whitespace)
        .map_or("", |(_, arg)| arg.trim());

    match command_name(text) {
        "/help" => Ok(HELP.into()),
        "/start" => Ok(HELP.into()),
        "/model" => model(runner, arg).await,
        "/models" => models(runner, arg).await,
        "/fallback" => fallback(runner).await,
        "/list" => list(runner).await,
        "/status" => status(runner).await,
        "/run" => match only_theme(runner, arg) {
            Ok(theme) => Ok(runner
                .trigger(TRIGGER_MANUAL, theme.as_ref())
                .await?
                .to_string()),
            Err(refusal) => Ok(refusal),
        },
        "/reread" => reread(runner, arg).await,
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
        "/rm" => guarded(runner, remove_many(runner, arg)).await,
        "/reset" => guarded(runner, reset(runner, arg)).await,
        "/skip" => guarded(runner, skip(runner, arg)).await,
        // Touches no cursors, but it sends: under the lock, so its parts never mix
        // with the parts of a digest that a run is sending to the same chat.
        "/last" => guarded(runner, last(runner, arg)).await,
        other => Ok(format!("unknown command {other}\n\n{HELP}")),
    }
}

/// The theme tag at the start of the argument, and the rest. No tag there: `None`
/// and the whole argument.
fn split_theme<'a>(runner: &Runner, arg: &'a str) -> (Option<Theme>, &'a str) {
    let (first, rest) = match arg.split_once(char::is_whitespace) {
        Some((first, rest)) => (first, rest.trim()),
        None => (arg, ""),
    };
    match runner.cfg.theme(first) {
        Some(theme) => (Some(theme), rest),
        None => (None, arg),
    }
}

/// For a command whose whole argument is a theme tag or nothing (`/run`, `/skip`,
/// `/last`). The `Err` is the reply for a word that is not a theme.
fn only_theme(runner: &Runner, arg: &str) -> std::result::Result<Option<Theme>, String> {
    if arg.is_empty() {
        return Ok(None);
    }
    match runner.cfg.theme(arg) {
        Some(theme) => Ok(Some(theme)),
        None => Err(format!(
            "there is no theme {arg}. Themes: {}",
            theme_tags(runner)
        )),
    }
}

fn theme_tags(runner: &Runner) -> String {
    let tags: Vec<String> = runner.cfg.themes().into_iter().map(|t| t.tag).collect();
    tags.join(", ")
}

/// The theme tag at the start of the argument of a command about channels already
/// in the database (`/rm`, `/reset`), and the rest. Besides the themes of the config
/// it knows the tags that only the database has: a theme that is gone from the
/// config still has its channels (`/list` shows them), and they must be removable.
async fn split_channel_theme<'a>(
    runner: &Runner,
    arg: &'a str,
) -> Result<(Option<String>, &'a str)> {
    let (theme, rest) = split_theme(runner, arg);
    if let Some(theme) = theme {
        return Ok((Some(theme.tag), rest));
    }
    if let Some((first, rest)) = arg.split_once(char::is_whitespace) {
        let known = runner.db.channels().await?;
        if known.iter().any(|c| c.theme == first) {
            return Ok((Some(first.to_string()), rest.trim()));
        }
    }
    Ok((None, arg))
}

/// The lines of the argument of a command that takes a list (`/add`, `/rm`). Telegram
/// sends several lines as one message, and each next line may start with the
/// command again: `/rm @a` and `/rm @b` on two lines remove both channels.
fn arg_lines<'a>(arg: &'a str, command: &'a str) -> impl Iterator<Item = &'a str> {
    arg.lines().map(move |line| {
        let line = line.trim();
        if command_name(line) == command {
            line.split_once(char::is_whitespace)
                .map_or("", |(_, rest)| rest.trim())
        } else {
            line
        }
    })
}

/// The channels in one line: separated by spaces or commas.
fn split_items(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
}

/// The row a command means by `[theme] channel`. Without a theme the channel must
/// be in one theme only. The inner `Err` is the reply when it is not clear.
async fn pick_channel(
    runner: &Runner,
    arg: &str,
    command: &str,
) -> Result<std::result::Result<Channel, String>> {
    let (tag, key) = split_channel_theme(runner, arg).await?;
    find_channel(runner, tag.as_deref(), key, command).await
}

/// The same as `pick_channel`, with the theme tag already split off.
async fn find_channel(
    runner: &Runner,
    tag: Option<&str>,
    key: &str,
    command: &str,
) -> Result<std::result::Result<Channel, String>> {
    if key.is_empty() {
        return Ok(Err(format!(
            "a channel is needed: {command} @name or {command} id"
        )));
    }
    // The same forms as `/add`: @name, name, t.me/name, https://t.me/name.
    let mut rows = runner.db.find_channels(username_from(key)).await?;
    if let Some(tag) = tag {
        rows.retain(|c| c.theme == tag);
    }
    // A row that is already turned off does not make the choice unclear.
    if rows.len() > 1 && rows.iter().any(|c| c.enabled) {
        rows.retain(|c| c.enabled);
    }
    if rows.len() > 1 {
        let tags: Vec<&str> = rows.iter().map(|c| c.theme.as_str()).collect();
        return Ok(Err(format!(
            "{key} is in the themes {}. Write the theme: {command} {} {key}",
            tags.join(", "),
            tags[0]
        )));
    }
    Ok(rows
        .pop()
        .ok_or_else(|| format!("channel {key} is not in the database, /list shows the list")))
}

/// Commands that touch cursors (and `/last`, which sends) go under the same lock
/// as the run. Each command that touches cursors
/// also checks for an undelivered run of ITS theme (`Runner::undelivered_refusal`):
/// that run's `runs.cursors` would overwrite the change on delivery. A run of
/// another theme does not block it.
async fn guarded<Fut>(runner: &Arc<Runner>, action: Fut) -> Result<String>
where
    Fut: std::future::Future<Output = Result<String>>,
{
    let Some(_guard) = runner.try_lock() else {
        return Ok(Outcome::Busy(runner.running_id().await?).to_string());
    };
    action.await
}

/// `/model` lists the models from the config with their limits, `/model provider/model`
/// switches to any model the provider has.
async fn model(runner: &Arc<Runner>, arg: &str) -> Result<String> {
    let current = runner.model().await;
    if arg.is_empty() {
        let cfg = &runner.cfg;
        let mut out = String::from("Models:");
        for key in cfg.model_keys() {
            let mark = if key == current { "✅" } else { "•" };
            // In backticks: monospace, one tap copies it for `/model`.
            out.push_str(&format!("\n{mark} `{key}`"));
            if key == current {
                out.push_str(" (now)");
            }
            if let Some(place) = cfg.fallback.iter().position(|m| *m == key) {
                out.push_str(&format!(" (fallback {})", place + 1));
            }
            match runner.catalog.spec(&key).await {
                Ok(spec) => out.push_str(&format!(
                    " — input {}k chars, output {}k tokens",
                    spec.max_input_chars / 1000,
                    spec.max_tokens / 1000
                )),
                Err(e) => out.push_str(&format!(" — {e:#}")),
            }
        }
        out.push_str(
            "\n\n/model provider/model — switch (any model of the provider)\n\
             /models provider — what the provider has",
        );
        return Ok(out);
    }
    match runner.set_model(arg).await {
        Ok(()) => Ok(format!("model: `{arg}` (was `{current}`)")),
        Err(e) => Ok(format!("cannot switch to {arg}: {e:#}")),
    }
}

/// `/models provider [word]`: the models from the provider API. The word keeps only
/// the ids that have it; `free` keeps the free models.
async fn models(runner: &Arc<Runner>, arg: &str) -> Result<String> {
    let mut words = arg.split_whitespace();
    let Some(provider) = words.next() else {
        let names: Vec<&String> = runner.cfg.providers.keys().collect();
        return Ok(format!(
            "write the provider: /models {}",
            names
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    };
    let filter = words.next().map(str::to_lowercase);
    let list = runner.catalog.refresh(provider).await?;
    let found: Vec<_> = list
        .iter()
        .filter(|m| match filter.as_deref() {
            None => true,
            Some("free") => m.free,
            Some(word) => m.id.to_lowercase().contains(word),
        })
        .collect();
    if found.is_empty() {
        return Ok(format!(
            "{provider}: no models found ({} in all)",
            list.len()
        ));
    }

    let mut out = format!("{provider}: {} models", found.len());
    for m in found {
        // The full key for `/model`, in backticks: one tap copies it.
        out.push_str(&format!(
            "
• `{provider}/{}`",
            m.id
        ));
        if let Some(tokens) = m.input_tokens {
            out.push_str(&format!(" — {}k", tokens / 1000));
        }
        if m.free {
            out.push_str(" free");
        }
        if m.json_schema == Some(false) {
            out.push_str(" (no strict JSON)");
        }
    }
    out.push_str(&format!(
        "

/model {provider}/<id> — switch"
    ));
    Ok(out)
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
    let outcome = runner.trigger(TRIGGER_MANUAL, None).await?;
    Ok(format!(
        "model: {fallback} (was {current}). /model {current} switches back.\n\n{outcome}"
    ))
}

async fn list(runner: &Arc<Runner>) -> Result<String> {
    let channels = runner.db.channels().await?;
    if channels.is_empty() {
        return Ok("no channels yet: add one with /add @channel".into());
    }

    // The themes in the config order, then the tags that only the database has.
    let mut tags: Vec<String> = runner.cfg.themes().into_iter().map(|t| t.tag).collect();
    let known = tags.len();
    for channel in &channels {
        if !tags.contains(&channel.theme) {
            tags.push(channel.theme.clone());
        }
    }

    // One message per theme: a long list of one theme does not mix with the next.
    let mut out = String::new();
    for (i, tag) in tags.iter().enumerate() {
        if i > 0 {
            out.push(MESSAGE_BREAK);
        }
        if tags.len() > 1 {
            let note = if i >= known {
                " (not in the config, not read)"
            } else {
                ""
            };
            out.push_str(&format!("— {tag}{note} —\n"));
        }
        let rows: Vec<&Channel> = channels.iter().filter(|c| c.theme == *tag).collect();
        if rows.is_empty() {
            out.push_str(&format!("no channels: /add {tag} @channel\n"));
        }
        for channel in rows {
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
    }
    Ok(out)
}

async fn status(runner: &Arc<Runner>) -> Result<String> {
    let mut out = format!("version {}", crate::version());
    let themes = runner.cfg.themes();
    let mut any = false;
    for theme in &themes {
        let Some(run) = runner.db.last_theme_run(&theme.tag).await? else {
            continue;
        };
        any = true;
        if themes.len() > 1 {
            out.push_str(&format!("\n\n— {} —", theme.tag));
        }
        out.push_str(&run_status(runner, &run).await?);
    }
    if !any {
        out.push_str("\nthere were no runs yet");
    }

    let channels = runner.db.channels().await?;
    let broken: Vec<&Channel> = channels.iter().filter(|c| c.last_error.is_some()).collect();
    if !broken.is_empty() {
        out.push_str("\n\nchannels with errors:");
        for channel in broken {
            out.push_str(&format!(
                "\n{} ({}) — {}",
                channel.label(),
                channel.theme,
                channel.last_error.as_deref().unwrap_or_default()
            ));
        }
    }
    Ok(out)
}

/// One run for `/status`: the numbers and the delivery of each part.
async fn run_status(runner: &Arc<Runner>, run: &Run) -> Result<String> {
    let chunks = runner.db.chunks(run.id).await?;
    let delivered = chunks.iter().filter(|c| c.delivered_at.is_some()).count();

    let mut out = format!(
        "\nrun #{} ({}): {}\nstarted {}\nfinished {}\nposts {}, topics {}\ntokens: {} / {}\nparts: {delivered} of {}\ndestination: {}",
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
    Ok(out)
}

/// The channel list lives only in the database, so `/add` writes there directly.
/// A channel that was removed with `/rm` comes back with a fresh cursor.
/// `/add` with several channels separated by spaces, commas or new lines; a new line
/// may start with `/add` again and with its own theme. Each one is added on its own:
/// an error in one channel does not stop the others.
async fn add_many(runner: &Arc<Runner>, arg: &str) -> Result<String> {
    let first = runner.cfg.themes().remove(0);
    let mut items: Vec<(Theme, &str)> = Vec::new();
    for line in arg_lines(arg, "/add") {
        let (theme, rest) = split_theme(runner, line);
        let theme = theme.unwrap_or_else(|| first.clone());
        items.extend(split_items(rest).map(|item| (theme.clone(), item)));
    }
    match items.as_slice() {
        // No channel: `add` answers with the usage.
        [] => add(runner, &first, "").await,
        [(theme, item)] => add_one(runner, theme, item).await,
        _ => {
            let mut out = Vec::with_capacity(items.len());
            for (theme, item) in &items {
                out.push(match add_one(runner, theme, item).await {
                    Ok(reply) => reply,
                    Err(e) => format!("{item}: {e:#}"),
                });
            }
            Ok(out.join("\n\n"))
        }
    }
}

/// One channel of `/add`, refused while its theme has an undelivered run.
async fn add_one(runner: &Arc<Runner>, theme: &Theme, item: &str) -> Result<String> {
    if let Some(refusal) = runner.undelivered_refusal(Some(&theme.tag)).await? {
        return Ok(format!("{item}: {refusal}"));
    }
    add(runner, theme, item).await
}

async fn add(runner: &Arc<Runner>, theme: &Theme, arg: &str) -> Result<String> {
    if arg.is_empty() {
        return Ok(
            "a channel is needed: /add @name, /add https://t.me/+invite or /add id. \
             With a theme: /add <theme> @name"
                .into(),
        );
    }
    let tag = theme.tag.as_str();

    // Already in the theme: say so and touch nothing, its cursor stays. For a username
    // or an id the database answers without a request to Telegram.
    if !is_invite(arg) {
        let rows = runner.db.find_channels(username_from(arg)).await?;
        if let Some(channel) = rows.iter().find(|c| c.theme == tag && c.enabled) {
            return Ok(already_added(channel));
        }
    }

    let peer = resolve_new_channel(runner, arg).await?;
    // An invite link is resolved only now, so check again by id.
    if let Some(channel) = runner.db.theme_channel(tag, peer.id).await? {
        if channel.enabled {
            return Ok(already_added(&channel));
        }
    }

    let (username, title) = channels::describe(&runner.client, peer).await?;
    runner
        .db
        .upsert_channel(tag, peer.id, peer.access_hash, username.as_deref(), &title)
        .await?;
    let place = if runner.cfg.themes().len() > 1 {
        format!(" to {tag}")
    } else {
        String::new()
    };

    // The account follows the channel: start from its read mark, so the posts it did
    // not read yet go into the digest.
    match channels::read_mark(&runner.client, peer).await {
        Ok(Some(mark)) => {
            runner.db.set_cursor(tag, peer.id, mark).await?;
            return Ok(format!(
                "added{place}: {title}, id {}. The digest starts from your read mark in Telegram \
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
        Ok(Some(last)) => runner.db.set_cursor(tag, peer.id, last).await?,
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), channel = peer.id, "could not read the last message, the lookback window is used")
        }
    }

    Ok(format!(
        "added{place}: {title}, id {}. Only new posts from now on go into the digest \
         (/reset {} reads the last lookback_hours too)",
        peer.id, peer.id
    ))
}

fn already_added(channel: &Channel) -> String {
    format!(
        "already in the list of {}: {}, id {}. /list shows all channels",
        channel.theme, channel.title, channel.id
    )
}

/// `/rm` with several channels: `/rm dev @a @b`, or several lines that each start
/// with `/rm` in one message (Telegram sends them as one message, and before this
/// the whole text was taken as one channel name, so nothing was removed).
async fn remove_many(runner: &Arc<Runner>, arg: &str) -> Result<String> {
    let mut keys: Vec<(Option<String>, &str)> = Vec::new();
    for line in arg_lines(arg, "/rm") {
        let (tag, rest) = split_channel_theme(runner, line).await?;
        keys.extend(split_items(rest).map(|item| (tag.clone(), item)));
    }
    match keys.as_slice() {
        // No channel: `remove` answers with the usage.
        [] => remove(runner, None, "").await,
        [(tag, key)] => remove(runner, tag.as_deref(), key).await,
        _ => {
            let mut out = Vec::with_capacity(keys.len());
            for (tag, key) in &keys {
                out.push(match remove(runner, tag.as_deref(), key).await {
                    Ok(reply) => reply,
                    Err(e) => format!("{key}: {e:#}"),
                });
            }
            Ok(out.join("\n"))
        }
    }
}

async fn remove(runner: &Arc<Runner>, tag: Option<&str>, key: &str) -> Result<String> {
    let channel = match find_channel(runner, tag, key, "/rm").await? {
        Ok(channel) => channel,
        Err(reply) => return Ok(reply),
    };
    if let Some(refusal) = runner.undelivered_refusal(Some(&channel.theme)).await? {
        return Ok(format!("{}: {refusal}", channel.label()));
    }

    // Disabled, not deleted: `/list` still shows the channel with its last error.
    runner
        .db
        .disable_channel(&channel.theme, channel.id)
        .await?;

    Ok(format!(
        "disabled in {}: {}, id {}",
        channel.theme, channel.title, channel.id
    ))
}

/// `/skip [theme]`: move every enabled channel to its newest message, so the next
/// digest has only posts that come after this. A channel that cannot be read keeps
/// its cursor.
async fn skip(runner: &Arc<Runner>, arg: &str) -> Result<String> {
    let theme = match only_theme(runner, arg) {
        Ok(theme) => theme,
        Err(refusal) => return Ok(refusal),
    };
    let tag = theme.as_ref().map(|t| t.tag.as_str());
    if let Some(refusal) = runner.undelivered_refusal(tag).await? {
        return Ok(refusal);
    }
    let channels = match tag {
        Some(tag) => runner.db.theme_channels(tag).await?,
        None => runner.db.enabled_channels().await?,
    };
    if channels.is_empty() {
        return Ok("no channels yet: add one with /add @channel".into());
    }

    let (mut moved, mut failed) = (0, Vec::new());
    for channel in &channels {
        match channels::last_message_id(&runner.client, channel.into()).await {
            Ok(Some(last)) => {
                runner
                    .db
                    .set_cursor(&channel.theme, channel.id, last)
                    .await?;
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
    let channel = match pick_channel(runner, arg, "/reset").await? {
        Ok(channel) => channel,
        Err(reply) => return Ok(reply),
    };
    if let Some(refusal) = runner.undelivered_refusal(Some(&channel.theme)).await? {
        return Ok(refusal);
    }
    runner.db.reset_channel(&channel.theme, channel.id).await?;
    Ok(format!(
        "{} ({}): cursor reset, the next run reads a window of {} h",
        channel.title, channel.theme, runner.cfg.telegram.lookback_hours
    ))
}

/// `/reread N [theme]`: the channels again for the last N days, then a run at once.
/// Takes `2`, `-2`, `2d` or `-2day`.
async fn reread(runner: &Arc<Runner>, arg: &str) -> Result<String> {
    const USAGE: &str = "write the number of days, 1 to 7: /reread 2 or /reread 2 <theme>";
    let mut words = arg.split_whitespace();
    let digits: String = words
        .next()
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    let days = match digits.parse::<i64>() {
        Ok(days) if (1..=7).contains(&days) => days,
        _ => return Ok(USAGE.into()),
    };
    let theme = match only_theme(runner, words.next().unwrap_or_default()) {
        Ok(theme) => theme,
        Err(refusal) => return Ok(refusal),
    };
    match runner.reread(days, theme.as_ref()).await? {
        Ok((count, outcome)) => Ok(format!("reread {days} d, {count} channels:\n{outcome}")),
        // The lock is taken or there is an undelivered run: the text says why.
        Err(refusal) => Ok(refusal),
    }
}

/// A deliberate repeat, not a recovery: the parts go to the **current**
/// destination of the theme and with new `random_id`.
async fn last(runner: &Arc<Runner>, arg: &str) -> Result<String> {
    let themes = match only_theme(runner, arg) {
        Ok(Some(theme)) => vec![theme],
        Ok(None) => runner.cfg.themes(),
        Err(refusal) => return Ok(refusal),
    };
    let bot = runner.bot()?;
    let mut out = Vec::new();
    for theme in &themes {
        let Some(run) = runner.db.last_ok_run(&theme.tag).await? else {
            out.push(format!(
                "{}: there were no published digests yet",
                theme.tag
            ));
            continue;
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
            out.push(format!("run #{} has no saved parts", run.id));
            continue;
        }

        let dest = publish::resolve_destination(bot, &theme.destination).await?;
        let peer = publish::destination_ref(dest)?;
        publish::resend(&bot.client, peer, &chunks, runner.cancellation()).await?;

        out.push(format!(
            "run #{} sent again to {} ({} parts)",
            run.id,
            theme.destination,
            chunks.len()
        ));
    }
    Ok(out.join("\n"))
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
            .map(|channel| channels::ChannelPeer::from(&channel));
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

    let peer = if is_invite(arg) {
        client
            .accept_invite_link(arg)
            .await
            .map_err(|e| {
                let text = e.to_string();
                if text.contains("INVITE_HASH_EXPIRED") || text.contains("INVITE_HASH_INVALID") {
                    anyhow!("the invite link is wrong or expired")
                } else {
                    anyhow!(e).context(format!("could not join by the link {arg}"))
                }
            })?
            .context("the link did not return a chat")?
    } else {
        // Telegram answers USERNAME_INVALID for a name that cannot exist and
        // USERNAME_NOT_OCCUPIED for a free one: for the owner both mean a typo.
        client
            .resolve_username(username_from(arg))
            .await
            .map_err(|e| {
                let text = e.to_string();
                if text.contains("USERNAME_INVALID") || text.contains("USERNAME_NOT_OCCUPIED") {
                    anyhow!("no such channel, check the name")
                } else {
                    anyhow!(e).context(format!("could not resolve {arg}"))
                }
            })?
            .context("no such channel, check the name")?
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
    matches!(
        command_name(text),
        "/run" | "/reread" | "/resume" | "/last" | "/skip" | "/add" | "/fallback"
    )
}

/// The command without its argument and without the bot name that Telegram may
/// add: `/list@my_digest_bot` is `/list`.
fn command_name(text: &str) -> &str {
    let word = text.split_whitespace().next().unwrap_or_default();
    word.split('@').next().unwrap_or_default()
}

/// A private invite link (`t.me/+…` or the old `joinchat`), not a username.
fn is_invite(arg: &str) -> bool {
    arg.contains("t.me/+") || arg.contains("joinchat")
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
