//! Loading `config.toml`: secrets, schedule, prompts, limits — everything comes from
//! this one file (in a container: `/data/config.toml`).
//!
//! There is no channel list here: it lives only in the database (`db.rs`) and is
//! managed with `/add` and `/rm` sent to the bot.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub schedule: Schedule,
    pub telegram: Telegram,
    /// Which model writes the digest, as `provider/model` from `[providers]`, for
    /// example "anthropic/claude-haiku-5-5". `/model` in the bot changes it; this is the default.
    pub model: String,
    /// Where to switch when the model is overloaded or its limit is used up, in this
    /// order. The bot asks the owner first (`/fallback`); nothing switches by itself.
    #[serde(default)]
    pub fallback: Vec<String>,
    /// Model providers with their keys and models: `[providers.<provider>]` and
    /// `[providers.<provider>.models.<name>]`.
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
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
}

impl Default for Bot {
    fn default() -> Self {
        Self {
            token: String::new(),
            session_file: default_bot_session_file(),
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

/// The API a provider speaks. The provider name in the config says which one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Anthropic,
    /// Google Gemini API. The key is from Google AI Studio: a Gemini app
    /// subscription (Google AI Pro / Ultra) does not give API access.
    Gemini,
    /// One key for many models, free ones too (ids end with `:free`). The model must
    /// support `response_format`; without a strict JSON schema we ask for a JSON object.
    OpenRouter,
}

/// All model providers the service knows, as they are written in the config.
pub const PROVIDERS: [&str; 3] = ["anthropic", "gemini", "openrouter"];

impl ProviderKind {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "anthropic" => Some(Self::Anthropic),
            "gemini" => Some(Self::Gemini),
            "openrouter" => Some(Self::OpenRouter),
            _ => None,
        }
    }

    /// For logs and messages to the owner.
    pub fn api_name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::Gemini => "Gemini",
            Self::OpenRouter => "OpenRouter",
        }
    }

    fn default_api_url(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com/v1/messages",
            // `{model}` is replaced with the model id.
            Self::Gemini => {
                "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
            }
            Self::OpenRouter => "https://openrouter.ai/api/v1/chat/completions",
        }
    }

    fn default_max_tokens(self) -> u32 {
        match self {
            Self::Gemini => 32000,
            _ => 16000,
        }
    }

    /// Many OpenRouter models have a context of 128k tokens or less.
    fn default_max_input_chars(self) -> usize {
        match self {
            Self::OpenRouter => 200_000,
            _ => 400_000,
        }
    }

    fn key_url(self) -> &'static str {
        match self {
            Self::Anthropic => "https://console.anthropic.com/settings/keys",
            Self::Gemini => "https://aistudio.google.com/apikey",
            Self::OpenRouter => "https://openrouter.ai/keys",
        }
    }
}

/// One provider: its key, the settings shared by its models, and the models.
#[derive(Debug, Deserialize)]
pub struct Provider {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub api_url: Option<String>,
    #[serde(flatten)]
    pub settings: ModelSettings,
    #[serde(default)]
    pub models: BTreeMap<String, Model>,
}

/// One model of a provider. Settings not set here come from the provider.
#[derive(Debug, Default, Deserialize)]
pub struct Model {
    /// The model id in the provider API, as it is (no date suffix is added). Not
    /// set: the model name from the config is the id.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(flatten)]
    pub settings: ModelSettings,
}

/// Settings that a model can have, or take from its provider.
#[derive(Debug, Default, Deserialize)]
pub struct ModelSettings {
    pub max_tokens: Option<u32>,
    /// Above this the input is condensed in parts (map-reduce).
    pub max_input_chars: Option<usize>,
    pub request_timeout_secs: Option<u64>,
    /// Anthropic only: low | medium | high | xhigh | max.
    pub effort: Option<String>,
    /// OpenRouter only: let the model think before it answers.
    pub reasoning: Option<bool>,
}

/// A model with all its settings in one place: what one run uses.
#[derive(Debug, Clone)]
pub struct ModelSpec {
    /// `provider/model`, as in the config and in `/model`.
    pub key: String,
    pub kind: ProviderKind,
    pub api_key: String,
    pub api_url: String,
    pub id: String,
    pub max_tokens: u32,
    pub max_input_chars: usize,
    pub request_timeout_secs: u64,
    pub effort: String,
    pub reasoning: bool,
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

    /// A model by its `provider/model`, with the provider settings filled in. Fails
    /// with a clear message if the model or the key is missing. Only the first `/`
    /// splits: OpenRouter ids have their own (`openrouter/google/gemma-4-31b-it:free`).
    pub fn model(&self, key: &str) -> Result<ModelSpec> {
        let (provider_name, model_name) = key.split_once('/').with_context(|| {
            format!("{key:?}: write the model as provider/model, for example anthropic/claude-haiku-5-5")
        })?;
        let kind = ProviderKind::from_name(provider_name).with_context(|| {
            format!("{key:?}: unknown provider {provider_name:?}, it must be one of {PROVIDERS:?}")
        })?;
        let provider = self
            .providers
            .get(provider_name)
            .with_context(|| format!("{key:?}: there is no [providers.{provider_name}]"))?;
        let model = provider.models.get(model_name).with_context(|| {
            format!("{key:?}: there is no [providers.{provider_name}.models.\"{model_name}\"]")
        })?;
        if provider.api_key.is_empty() {
            bail!(
                "providers.{provider_name}.api_key is not set (get it at {})",
                kind.key_url()
            );
        }

        let (m, p) = (&model.settings, &provider.settings);
        Ok(ModelSpec {
            key: key.to_string(),
            kind,
            api_key: provider.api_key.clone(),
            api_url: provider
                .api_url
                .clone()
                .unwrap_or_else(|| kind.default_api_url().to_string()),
            id: model.id.clone().unwrap_or_else(|| model_name.to_string()),
            max_tokens: m
                .max_tokens
                .or(p.max_tokens)
                .unwrap_or_else(|| kind.default_max_tokens()),
            max_input_chars: m
                .max_input_chars
                .or(p.max_input_chars)
                .unwrap_or_else(|| kind.default_max_input_chars()),
            request_timeout_secs: m
                .request_timeout_secs
                .or(p.request_timeout_secs)
                .unwrap_or(600),
            effort: m
                .effort
                .clone()
                .or_else(|| p.effort.clone())
                .unwrap_or_else(|| "high".into()),
            reasoning: m.reasoning.or(p.reasoning).unwrap_or(false),
        })
    }

    /// All models in the config as `provider/model`.
    pub fn model_keys(&self) -> Vec<String> {
        self.providers
            .iter()
            .flat_map(|(provider, p)| p.models.keys().map(move |m| format!("{provider}/{m}")))
            .collect()
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
        for name in self.providers.keys() {
            if ProviderKind::from_name(name).is_none() {
                bail!("[providers.{name}]: unknown provider, it must be one of {PROVIDERS:?}");
            }
        }
        self.model(&self.model).context("model")?;
        for key in &self.fallback {
            self.model(key).context("fallback")?;
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
