//! Controlling the service with commands in Saved Messages.
//!
//! We listen to updates and react **only** to outgoing messages in the chat with
//! ourselves: nobody except the account owner can write there, so no separate
//! authorization is needed. We do not create a bot for this — a bot cannot read
//! channels anyway, and the account itself gives a UI for one owner.
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
/add @channel | link | id — add a channel
/rm @channel | id — remove a channel
/list — channels, cursors, errors
/reset id — read the channel again from the lookback_hours window
/run — run now (or deliver an undelivered digest)
/status — last run: status, posts, tokens
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
    let me = runner
        .client
        .get_me()
        .await
        .context("could not get own profile")?
        .id();

    let mut stream = runner
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

    tracing::info!("listening for commands in Saved Messages, /help for the list");
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
            continue;
        };
        if !message.outgoing() || message.peer_id() != me {
            continue;
        }
        let text = message.text().trim().to_string();
        if !text.starts_with('/') {
            continue;
        }

        tracing::info!(command = %text, "command from Saved Messages");
        let runner = runner.clone();
        let cancel = cancel.clone();
        commands.spawn(async move {
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
    let command = parts.next().unwrap_or_default();
    let arg = parts.next().unwrap_or("").trim();

    match command {
        "/help" | "/start" => Ok(HELP.into()),
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
        "/add" => guarded(runner, add(runner, arg)).await,
        "/rm" => guarded(runner, remove(runner, arg)).await,
        "/reset" => guarded(runner, reset(runner, arg)).await,
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
    let Some(run) = runner.db.last_run().await? else {
        return Ok("there were no runs yet".into());
    };

    let chunks = runner.db.chunks(run.id).await?;
    let delivered = chunks.iter().filter(|c| c.delivered_at.is_some()).count();

    let mut out = format!(
        "run #{} ({}): {}\nstarted {}\nfinished {}\nposts {}, topics {}\ntokens: {} / {}\nparts: {delivered} of {}\ndestination: {}",
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
async fn add(runner: &Arc<Runner>, arg: &str) -> Result<String> {
    if arg.is_empty() {
        return Ok("a channel is needed: /add @name, /add https://t.me/+invite or /add id".into());
    }

    let peer = resolve_new_channel(runner, arg).await?;
    let (username, title) = channels::describe(&runner.client, peer).await?;
    runner
        .db
        .upsert_channel(peer.id, peer.access_hash, username.as_deref(), &title)
        .await?;

    Ok(format!("added: {title}, id {}", peer.id))
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
        .collect();
    if chunks.is_empty() {
        return Ok(format!("run #{} has no saved parts", run.id));
    }

    let dest =
        publish::resolve_destination(&runner.client, &runner.cfg.telegram.destination).await?;
    publish::resend(
        &runner.client,
        publish::destination_ref(dest)?,
        &chunks,
        runner.cancellation(),
    )
    .await?;

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
            .resolve_username(arg.trim_start_matches('@'))
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

fn peer_kind(peer: &Peer) -> &'static str {
    match peer {
        Peer::User(user) if user.is_bot() => "bot",
        Peer::User(_) => "user",
        Peer::Group(_) => "group",
        Peer::Channel(_) => "channel",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, Destination};
    use grammers_client::{Client, SenderPool};
    use grammers_session::storages::MemorySession;
    use std::collections::BTreeMap;

    async fn runner() -> Arc<Runner> {
        let cfg = toml::from_str(include_str!("../../config.example.toml")).unwrap();
        let db = Arc::new(Db::open(":memory:").await.unwrap());
        // The sender is never run: all commands in these tests are local.
        let pool = SenderPool::new(Arc::new(MemorySession::default()), 1);
        Arc::new(Runner::new(
            Arc::new(cfg),
            db,
            Client::new(pool.handle),
            Cancel::never(),
        ))
    }

    #[tokio::test]
    async fn commands_reject_mutation_but_allow_reads_during_a_run() {
        let runner = runner().await;
        let _guard = runner.try_lock().unwrap();
        for command in [
            "/run",
            "/reset 1",
            "/abandon 1",
            "/resume 1",
            "/add @news",
            "/rm 1",
            "/last",
        ] {
            let reply = handle(&runner, command).await.unwrap();
            assert!(reply.contains("is going"), "{command}: {reply}");
        }
        assert!(handle(&runner, "/list").await.is_ok());
        assert!(handle(&runner, "/status").await.is_ok());
        assert_eq!(handle(&runner, "/help").await.unwrap(), HELP);
    }

    #[tokio::test]
    async fn pending_delivery_blocks_reset_until_abandoned() {
        let runner = runner().await;
        runner.db.upsert_channel(1, 7, None, "News").await.unwrap();
        let id = runner.db.start_run("manual").await.unwrap();
        runner
            .db
            .save_summary(
                id,
                1,
                1,
                (None, None),
                &BTreeMap::from([(1, 42)]),
                "me",
                Destination {
                    peer_id: 5,
                    access_hash: 6,
                },
                &[("news".into(), 7)],
            )
            .await
            .unwrap();
        assert!(handle(&runner, "/reset 1")
            .await
            .unwrap()
            .contains("undelivered"));
        handle(&runner, &format!("/abandon {id}")).await.unwrap();
        assert!(handle(&runner, "/reset 1")
            .await
            .unwrap()
            .contains("cursor reset"));
        assert!(runner.db.unfinished_run().await.unwrap().is_none());
        assert_eq!(runner.db.channel(1).await.unwrap().unwrap().last_msg_id, 0);
    }
}
