//! Connection, login and post selection.
//!
//! WARNING: grammers 0.10 rewrote the connection scheme compared to 0.7–0.9:
//! now a `SenderPool` (session + api_id) is created first, its `runner`
//! is spawned as a separate task, `Client::new` takes `handle`, and updates
//! come through a separate receiver of the pool. The session is a SQLite file, not the old
//! `Session::load_file_or_create`.
//! Check signatures at https://docs.rs/grammers-client/0.10.0/

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
use grammers_session::Session as _;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::cancel::Cancel;
use crate::config::Config;
use crate::db::Channel;
use crate::digest::Post;

/// How many times we retry reading a channel after a temporary error.
const MAX_ATTEMPTS: u32 = 4;
/// We do not wait out a FLOOD_WAIT longer than this: the channel is excluded from the run,
/// its cursor does not move, the digest is built from the other channels.
const MAX_FLOOD_WAIT: Duration = Duration::from_secs(600);

/// A connected client together with its session and updates receiver. The pool has
/// exactly one receiver, so whoever listens for updates takes it from here.
pub struct Session {
    pub client: Client,
    pub session: Arc<SqliteSession>,
    pub updates: UnboundedReceiver<UpdatesLike>,
}

/// Connect the personal account: it reads the channels.
pub async fn connect(cfg: &Config) -> Result<Session> {
    open(&cfg.telegram.session_file, cfg.telegram.api_id).await
}

/// Open a session file, connect to Telegram and start the background task of the
/// network pool. Used for the personal account and for the bot.
pub async fn open(session_file: &str, api_id: i32) -> Result<Session> {
    let session = Arc::new(
        SqliteSession::open(session_file)
            .await
            .with_context(|| format!("could not open the session {session_file}"))?,
    );

    // Without the own user in the session cache, grammers does not ask for the update
    // state, and the command listener gets no updates.
    let self_known = session
        .peer(PeerId::self_user())
        .await
        .ok()
        .flatten()
        .is_some();
    tracing::debug!(session_file, self_known, "session opened");

    let pool = SenderPool::new(session.clone(), api_id);
    let SenderPool {
        runner,
        handle,
        updates,
    } = pool;

    // The pool does nothing by itself — it must be run.
    tokio::spawn(runner.run());

    Ok(Session {
        client: Client::new(handle),
        session,
        updates,
    })
}

/// Interactive login: the code from Telegram + the 2FA password if it is on.
pub async fn login(client: &Client, cfg: &Config) -> Result<()> {
    if client.is_authorized().await? {
        println!("Already logged in, no need to log in again.");
        return Ok(());
    }

    let phone = &cfg.telegram.phone;
    println!("Requesting a code for {phone}…");

    let token = client
        .request_login_code(phone, &cfg.telegram.api_hash)
        .await
        .context("could not request a login code")?;

    let code = prompt("Code from Telegram: ")?;

    match client.sign_in(&token, code.trim()).await {
        Ok(user) => {
            println!("Done, logged in as {}", user.id());
            Ok(())
        }
        // Without this branch accounts with two-factor authentication cannot log in.
        Err(SignInError::PasswordRequired(password_token)) => {
            let password = rpassword::prompt_password("2FA password: ")
                .context("could not read the password")?;
            let user = client
                .check_password(password_token, password.trim())
                .await
                .context("wrong 2FA password")?;
            println!("Done, logged in as {}", user.id());
            Ok(())
        }
        Err(e) => Err(anyhow!("login failed: {e}")),
    }
}

pub async fn ensure_authorized(client: &Client) -> Result<()> {
    if !client.is_authorized().await? {
        bail!("not logged in to Telegram — run `tg-digest login`");
    }
    Ok(())
}

/// The session is revoked ("Terminate all other sessions", a deleted account).
///
/// This is not "an error of one run": there is no point in waiting it out, so the daemon
/// exits with a non-zero code on this error and asks to run `login`.
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

/// The same, but for an error already wrapped in `anyhow`: a run does not survive
/// such errors, the daemon must exit.
pub fn is_fatal(error: &anyhow::Error) -> bool {
    error.chain().any(|e| {
        e.downcast_ref::<InvocationError>()
            .is_some_and(is_auth_error)
    })
}

/// How long FLOOD_WAIT asks to wait, if this is FLOOD_WAIT.
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
        PeerId::channel(id).ok_or_else(|| anyhow!("{id} does not look like a channel id"))?;
    Ok(PeerRef {
        id: peer_id,
        auth: PeerAuth::from_hash(access_hash),
    })
}

/// The result of collecting from all channels.
pub struct Collected {
    pub posts: Vec<Post>,
    /// Cursors of **successfully read** channels: the highest id we saw,
    /// regardless of the length filter and album merging.
    pub cursors: BTreeMap<i64, i32>,
    /// Channels excluded from the run, and the reason. Their cursor does not move.
    pub failures: Vec<(Channel, String)>,
}

/// Collect posts of the enabled channels.
///
/// The limit is the channel cursor, not a global time: a channel that failed for three
/// days gives all three days when it works again. The date matters only when
/// the cursor is 0 (a new or reset channel).
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
            bail!("collection stopped by signal");
        }
        if index > 0 {
            // A pause between channels is the cheapest protection from FLOOD_WAIT.
            if cancel.sleep(pause).await.is_err() {
                bail!("collection stopped by signal");
            }
        }

        match fetch_channel(client, cfg, channel, since, cancel).await {
            Ok(batch) => {
                tracing::info!(
                    channel = %channel.label(),
                    count = batch.posts.len(),
                    cursor = batch.max_id_seen,
                    "posts collected"
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
            Err(ChannelError::Cancelled) => bail!("collection stopped by signal"),
            Err(ChannelError::Auth(e)) => return Err(e).context("Telegram session is not valid"),
            Err(ChannelError::Excluded(reason)) => {
                tracing::warn!(channel = %channel.label(), %reason, "channel excluded from the run");
                collected.failures.push((channel.clone(), reason));
            }
        }
    }

    Ok(collected)
}

enum ChannelError {
    /// The channel drops out of this run; we do not move the cursor, the digest is built
    /// from the other channels.
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
    // +1: to tell "the channel ended exactly at the limit" from "we hit the limit".
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
                        "error reading the channel: {e}. The next run reads from the cursor"
                    )));
                }
                let delay = wait.unwrap_or_else(|| Duration::from_secs(2u64.pow(attempt)));
                tracing::warn!(channel = %channel.label(), error = %e, ?delay, "retrying the read");
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

        // We hit the safety limit before reaching the cursor: we cannot silently cut
        // the tail, so the channel drops out of the run completely.
        if raw.len() >= limit {
            return Err(ChannelError::Excluded(format!(
                "limit of {limit} posts per run exceeded — raise telegram.max_posts_per_channel \
                 or reset the channel with /reset {}",
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

/// The collection limit. With cursor 0 (a new or reset channel) we take a window by
/// date, otherwise we read everything above the cursor, however many days that is.
fn reached_border(id: i32, date: DateTime<Utc>, cursor: i32, since: DateTime<Utc>) -> bool {
    if cursor > 0 {
        id <= cursor
    } else {
        date < since
    }
}

/// A message before album merging and the filter — in this form it is easy to use
/// in tests without Telegram.
#[derive(Debug, Clone)]
pub struct RawMessage {
    pub id: i32,
    pub date: DateTime<Utc>,
    pub grouped_id: Option<i64>,
    pub text: String,
    /// Where the repost is from, if it is a repost.
    pub forward: Option<Forward>,
}

/// The origin of a repost. It is needed so that duplicate merging does not see a repost
/// and the original as two different news items: without the label the model honestly
/// gives two items about one event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Forward {
    /// Name of the source channel, if Telegram sent it (hidden ones have none).
    pub name: Option<String>,
    /// Id of the source channel: we use it to find the title among our channels.
    pub channel_id: Option<i64>,
    /// Post number in the source channel.
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
    /// The highest id we saw, **before** the filter: otherwise parts of an album
    /// stay above the cursor and are read again on the next run.
    pub max_id_seen: i32,
}

/// Album merging and the short post filter.
///
/// An album = several messages with one `grouped_id`, but one publication:
/// the text is in the caption of one of the parts, we link to the earliest one.
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
                // The whole album is one forward, so we take the source
                // from any part that has it.
                if entry.forward.is_none() {
                    entry.forward = message.forward.clone();
                }
                // The link must lead to the first part of the album.
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

    // Short posts are stickers, reactions and "media only": noise for the digest.
    posts.retain(|p| p.text.chars().count() >= min_post_chars);
    posts.sort_by_key(|p| p.msg_id);

    Batch { posts, max_id_seen }
}

/// A human-readable source of a repost.
///
/// If the source channel is one of ours, we take its title and a link to the
/// original post: then the model sees that the repost and the original are one
/// event, not two.
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
            Some(id) => format!("channel id {id}"),
            None => "hidden source".into(),
        },
    }
}

/// Where the repost is from. Hidden forwards have neither id nor name — then `None`.
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

/// `message.text()` gives plain text: the link from "more — [here]" is lost.
/// We add such addresses as a separate line — the model needs them for context,
/// but only links to the posts themselves go into `sources` anyway.
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
    format!("{text}\nLinks in the post: {}", urls.join(", "))
}

fn prompt(label: &str) -> Result<String> {
    use std::io::{stdin, stdout, Write};
    print!("{label}");
    stdout().flush()?;
    let mut buf = String::new();
    stdin().read_line(&mut buf)?;
    Ok(buf)
}
