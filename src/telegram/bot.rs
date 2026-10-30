//! The bot: the owner talks to it. It gets commands, answers them, and posts digests
//! and notifications.
//!
//! The bot reads no channels: Telegram does not let a bot read a channel where it is
//! not an admin. Reading is done by the personal account (`client.rs`).

use anyhow::{anyhow, bail, Context, Result};
use grammers_client::client::Client;
use grammers_client::message::InputMessage;
use grammers_session::storages::SqliteSession;
use grammers_session::types::{PeerId, PeerRef};
use grammers_session::updates::UpdatesLike;
use grammers_session::Session as _;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::config::Config;
use crate::telegram::client;

/// Shown when the bot does not know the owner yet.
const SEND_START: &str =
    "the bot cannot write to you yet: open the bot in Telegram and send it /start";

pub struct Bot {
    pub client: Client,
    session: Arc<SqliteSession>,
    /// The owner is the personal account from `[telegram]`. Only this user may give
    /// commands, and the bot writes only to this user (or to a channel).
    pub owner: PeerId,
    /// Who may give commands: the owner and `bot.allowed_users`.
    allowed: Vec<PeerId>,
}

/// Connect the bot and log in with the token. A bot needs no code and no password,
/// so this happens on start, without `login`.
pub async fn connect(cfg: &Config, owner: PeerId) -> Result<(Bot, UnboundedReceiver<UpdatesLike>)> {
    let token = cfg.bot.token.trim();
    if token.is_empty() {
        bail!(
            "bot.token is not set: create a bot with @BotFather and put its token \
             into [bot] in config.toml"
        );
    }

    let session = client::open(&cfg.bot.session_file, cfg.telegram.api_id).await?;
    let authorized = session
        .client
        .is_authorized()
        .await
        .context("could not check the bot login")?;
    if !authorized {
        let me = session
            .client
            .bot_sign_in(token, &cfg.telegram.api_hash)
            .await
            .context("bot login failed: check bot.token")?;
        tracing::info!(bot = ?me.username(), "bot logged in");
    }

    let mut allowed = vec![owner];
    for &id in &cfg.bot.allowed_users {
        match PeerId::user(id) {
            Some(peer) => allowed.push(peer),
            None => bail!("bot.allowed_users: {id} is not a Telegram user id"),
        }
    }

    let bot = Bot {
        client: session.client,
        session: session.session,
        owner,
        allowed,
    };
    Ok((bot, session.updates))
}

impl Bot {
    /// The chat with the owner.
    ///
    /// A bot can write to a user only after the user wrote to it: the user's
    /// `access_hash` comes in that message, and grammers keeps it in the bot session.
    /// So it survives restarts, `/start` is needed only once.
    pub async fn owner_ref(&self) -> Result<PeerRef> {
        let info = self
            .session
            .peer(self.owner)
            .await
            .map_err(|e| anyhow!("could not read the bot session: {e}"))?
            .ok_or_else(|| anyhow!(SEND_START))?;
        let auth = info.auth().ok_or_else(|| anyhow!(SEND_START))?;
        Ok(PeerRef {
            id: self.owner,
            auth,
        })
    }

    /// Whether this user may give commands.
    pub fn is_allowed(&self, user: PeerId) -> bool {
        self.allowed.contains(&user)
    }

    /// A short notification to the owner.
    ///
    /// This is the cheapest monitoring possible: otherwise we learn about a failed
    /// night run from the logs a week later. The notification itself must not break
    /// the run — errors go only to the log.
    pub async fn notify(&self, text: &str) {
        if let Err(e) = self.try_notify(text).await {
            tracing::warn!(error = %format!("{e:#}"), %text, "could not send a notification");
        }
    }

    async fn try_notify(&self, text: &str) -> Result<()> {
        let peer = self.owner_ref().await?;
        // An error text can be longer than the message limit, and Telegram rejects that:
        // a failure notification must not get lost because of its own size.
        for piece in crate::digest::split_plain(text) {
            self.client
                .send_message(peer, InputMessage::new().text(piece).link_preview(false))
                .await?;
        }
        Ok(())
    }
}
