//! Loading `config.toml`: secrets, schedule, prompts, limits — everything comes from
//! this one file (in a container: `/data/config.toml`).
//!
//! There is no channel list here: it lives only in the database (`db.rs`) and is
//! managed with `/add` and `/rm` sent to the bot.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub schedule: Schedule,
    pub telegram: Telegram,
    /// Which model writes the digest: "anthropic" (default) or "gemini".
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Where to switch when the limit of the current provider is used up. The bot
    /// asks the owner first (`/fallback`); nothing switches by itself.
    #[serde(default)]
    pub fallback_provider: Option<String>,
    #[serde(default)]
    pub anthropic: Anthropic,
    #[serde(default)]
    pub gemini: Gemini,
    pub prompts: Prompts,
    /// The bot you talk to: commands, replies, digests, notifications.
    #[serde(default)]
    pub bot: Bot,
    /// Database file. In a container it is in the `/data` volume, next to the session.
    #[serde(default = "default_db_file")]
    pub db_file: String,
}

/// The bot reads no channels (a bot cannot), it only talks to the owner and posts.
/// Reading is done by the personal account from `[telegram]`.
#[derive(Debug, Deserialize)]
pub struct Bot {
    /// Token from @BotFather.
    #[serde(default)]
    pub token: String,
    /// The bot session (SQLite). It is created on the first start, no login needed.
    #[serde(default = "default_bot_session_file")]
    pub session_file: String,
    /// Other Telegram user ids that may give the bot commands. The owner (the account
    /// from `[telegram]`) is always allowed and is not listed here.
    #[serde(default)]
    pub allowed_users: Vec<i64>,
}

impl Default for Bot {
    fn default() -> Self {
        Self {
            token: String::new(),
            session_file: default_bot_session_file(),
            allowed_users: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Schedule {
    pub cron: String,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default)]
    pub catch_up: bool,
    #[serde(default)]
    pub run_on_start: bool,
}

#[derive(Debug, Deserialize)]
pub struct Telegram {
    #[serde(default)]
    pub api_id: i32,
    #[serde(default)]
    pub api_hash: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default = "default_session_file")]
    pub session_file: String,
    /// Where the bot posts the digest: `"bot"` — the chat with the bot (`"me"` means the
    /// same, for old configs), or `"@channel"` where the bot is an admin.
    #[serde(default = "default_destination")]
    pub destination: String,
    #[serde(default = "default_lookback_hours")]
    pub lookback_hours: i64,
    /// A safety limit against reading the whole history, **not** a digest cut: a channel
    /// that hits the limit is excluded from the run completely and its cursor does not move.
    #[serde(default = "default_max_posts")]
    pub max_posts_per_channel: usize,
    #[serde(default = "default_min_post_chars")]
    pub min_post_chars: usize,
    #[serde(default = "default_pause_ms")]
    pub pause_between_channels_ms: u64,
    /// How many channels are read at the same time.
    #[serde(default = "default_parallel_channels")]
    pub parallel_channels: usize,
    /// After a delivered (or empty) run, mark the read posts as read in Telegram, so
    /// the channels do not keep unread counters for news the digest already told.
    #[serde(default = "default_true")]
    pub mark_read: bool,
    /// For how many hours the daemon itself retries an undelivered digest. After this
    /// window the retry is manual only (`/resume`), see docs/PLAN.md 9.3.
    #[serde(default = "default_publish_retry_window_hours")]
    pub publish_retry_window_hours: i64,
}

#[derive(Debug, Deserialize)]
pub struct Anthropic {
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_effort")]
    pub effort: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_api_url")]
    pub api_url: String,
    #[serde(default = "default_timeout_secs")]
    pub request_timeout_secs: u64,
}

impl Default for Anthropic {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: default_model(),
            effort: default_effort(),
            max_tokens: default_max_tokens(),
            api_url: default_api_url(),
            request_timeout_secs: default_timeout_secs(),
        }
    }
}

/// Google Gemini API. The key is from Google AI Studio: a Gemini app
/// subscription (Google AI Pro / Ultra) does not give API access.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Gemini {
    pub api_key: String,
    pub model: String,
    pub max_tokens: u32,
    /// `{model}` is replaced with the model id.
    pub api_url: String,
    pub request_timeout_secs: u64,
}

impl Default for Gemini {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: "gemini-2.5-flash".into(),
            max_tokens: 32000,
            api_url:
                "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
                    .into(),
            request_timeout_secs: default_timeout_secs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Prompts {
    pub system: String,
    pub user_template: String,
    /// Instruction for the map step, when the input does not fit in one request.
    /// Not set — `summarize::DEFAULT_MAP_SYSTEM` is used.
    #[serde(default)]
    pub map_system: Option<String>,
    /// Digest title; `{{date}}` is the date in the time zone from `[schedule]`.
    #[serde(default = "default_digest_title")]
    pub digest_title: String,
    #[serde(default = "default_tldr_title")]
    pub tldr_title: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| {
            format!(
                "could not read {}. Copy config.example.toml to config.toml and fill it in",
                path.display()
            )
        })?;

        let mut cfg: Config =
            toml::from_str(&raw).with_context(|| format!("{} — invalid TOML", path.display()))?;

        cfg.telegram.phone = normalize_phone(&cfg.telegram.phone);
        cfg.validate()?;
        Ok(cfg)
    }

    /// The provider name is known and its key is set.
    pub fn check_provider(&self, name: &str) -> Result<()> {
        match name {
            "anthropic" if self.anthropic.api_key.is_empty() => {
                bail!("anthropic.api_key is not set")
            }
            "gemini" if self.gemini.api_key.is_empty() => {
                bail!("gemini.api_key is not set (get it at https://aistudio.google.com/apikey)")
            }
            "anthropic" | "gemini" => Ok(()),
            other => bail!("must be \"anthropic\" or \"gemini\", not {other:?}"),
        }
    }

    /// We catch empty required fields here, not on the first network call:
    /// otherwise the error shows up in the middle of a night run as a 401.
    fn validate(&self) -> Result<()> {
        if self.telegram.api_id == 0 || self.telegram.api_hash.is_empty() {
            bail!("telegram.api_id / telegram.api_hash are not set (get them at https://my.telegram.org)");
        }
        if self.telegram.phone.is_empty() {
            bail!("telegram.phone is not set");
        }
        self.check_provider(&self.provider).context("provider")?;
        if let Some(fallback) = &self.fallback_provider {
            self.check_provider(fallback).context("fallback_provider")?;
        }
        if self.telegram.lookback_hours <= 0 {
            bail!("telegram.lookback_hours must be greater than zero");
        }
        if self.telegram.parallel_channels == 0 {
            bail!("telegram.parallel_channels must be greater than zero");
        }
        if self.telegram.max_posts_per_channel == 0 {
            bail!("telegram.max_posts_per_channel must be greater than zero");
        }
        if self.telegram.publish_retry_window_hours < 0 {
            bail!("telegram.publish_retry_window_hours cannot be negative");
        }
        Ok(())
    }
}

fn default_timezone() -> String {
    "UTC".into()
}
fn default_session_file() -> String {
    "/data/tg.session".into()
}
fn default_db_file() -> String {
    "/data/digest.db".into()
}
fn default_destination() -> String {
    "bot".into()
}
fn default_bot_session_file() -> String {
    "/data/bot.session".into()
}
fn default_lookback_hours() -> i64 {
    24
}
fn default_max_posts() -> usize {
    1000
}
fn default_min_post_chars() -> usize {
    30
}
fn default_true() -> bool {
    true
}
fn default_parallel_channels() -> usize {
    5
}
fn default_pause_ms() -> u64 {
    1500
}
fn default_publish_retry_window_hours() -> i64 {
    2
}
fn default_provider() -> String {
    "anthropic".into()
}
fn default_model() -> String {
    "claude-opus-5".into()
}
fn default_effort() -> String {
    "high".into()
}
fn default_max_tokens() -> u32 {
    16000
}
fn default_api_url() -> String {
    "https://api.anthropic.com/v1/messages".into()
}
fn default_timeout_secs() -> u64 {
    600
}
fn default_digest_title() -> String {
    "📰 Digest for {{date}}".into()
}
fn default_tldr_title() -> String {
    "In short".into()
}

/// Telegram expects the number as digits: we remove dashes, spaces and brackets from
/// the config ourselves, or `login` fails with PHONE_NUMBER_INVALID for no reason.
/// We keep the leading `+` if it was there.
fn normalize_phone(raw: &str) -> String {
    let trimmed = raw.trim();
    let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();

    if digits.is_empty() {
        return String::new();
    }
    if trimmed.starts_with('+') {
        format!("+{digits}")
    } else {
        digits
    }
}
