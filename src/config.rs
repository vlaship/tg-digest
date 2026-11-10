//! Loading `config.toml`: secrets, schedule, prompts, limits — everything comes from
//! this one file (in a container: `/data/config.toml`).
//!
//! There is no channel list here: it lives only in the database (`db.rs`) and is
//! managed with `/add` and `/rm` sent to the bot.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

use crate::models::Entry;

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
    /// `[providers.<provider>.models.<name>]`. A model table is needed only to change
    /// a setting: the limits come from the provider's own model list.
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
    pub prompts: Prompts,
    /// `[[themes]]`: independent digests, each with its own channels and system
    /// prompt. They run one after another, in this order. Empty: one theme
    /// (`DEFAULT_THEME`) made from `[prompts]`.
    #[serde(default)]
    pub themes: Vec<ThemeConfig>,
    /// Database file. In a container it is in the `/data` volume, next to the session.
    #[serde(default = "default_db_file")]
    pub db_file: String,
}

/// `[telegram.bot]`: the bot you talk to. It reads no channels (a bot cannot), it only
/// talks to the owner and posts. Reading is done by the personal account (`[telegram.app]`).
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

/// `[telegram.app]`: the personal account and the Telegram app keys. The account
/// only reads: it collects posts, resolves `/add`, refreshes names. The bot logs in
/// with the same `api_id` and `api_hash`.
#[derive(Debug, Deserialize)]
pub struct App {
    #[serde(default)]
    pub api_id: i32,
    #[serde(default)]
    pub api_hash: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default = "default_session_file")]
    pub session_file: String,
}

impl Default for App {
    fn default() -> Self {
        Self {
            api_id: 0,
            api_hash: String::new(),
            phone: String::new(),
            session_file: default_session_file(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Telegram {
    #[serde(default)]
    pub app: App,
    /// The bot you talk to: commands, replies, digests, notifications.
    #[serde(default)]
    pub bot: Bot,
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

/// Characters per token, times ten. Russian text is about 2.5–3, so 1.8 leaves room
/// for the prompt and for text with many links.
const CHARS_PER_TOKEN_X10: u64 = 18;

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
    /// OpenAI API (`platform.openai.com`), paid per use. A ChatGPT subscription gives
    /// no API access.
    OpenAI,
}

/// All model providers the service knows, as they are written in the config.
pub const PROVIDERS: [&str; 4] = ["anthropic", "gemini", "openrouter", "openai"];

impl ProviderKind {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "anthropic" => Some(Self::Anthropic),
            "gemini" => Some(Self::Gemini),
            "openrouter" => Some(Self::OpenRouter),
            "openai" => Some(Self::OpenAI),
            _ => None,
        }
    }

    /// For logs and messages to the owner.
    pub fn api_name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::Gemini => "Gemini",
            Self::OpenRouter => "OpenRouter",
            Self::OpenAI => "OpenAI",
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
            Self::OpenAI => "https://api.openai.com/v1/chat/completions",
        }
    }

    /// Also the most we ask for, even if the model allows more. Anthropic: thinking
    /// with `effort = "high"` uses the same limit as the answer, and 16000 was cut on
    /// 156k characters of input; only the used tokens are billed. Others: a digest is
    /// never that long, and some OpenRouter hosts reserve the whole `max_tokens`.
    fn default_max_tokens(self) -> u32 {
        match self {
            Self::Anthropic => 64000,
            _ => 32000,
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
            Self::OpenAI => "https://platform.openai.com/api-keys",
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
    /// Anthropic: low | medium | high | xhigh | max. OpenAI: sent as `reasoning_effort`
    /// (none | minimal | low | medium | high), only when it is set.
    pub effort: Option<String>,
    /// OpenRouter only: let the model think before it answers.
    pub reasoning: Option<bool>,
    /// OpenRouter only: how to choose the host of the model: price | throughput |
    /// latency. Not set: OpenRouter balances price and uptime.
    pub sort: Option<String>,
    /// OpenRouter only: hosts that never get the request, by name ("Relace").
    pub ignore: Option<Vec<String>>,
    /// OpenRouter only: only hosts that run the model in one of these forms
    /// ("fp8", "bf16"). A strongly compressed model (fp4) can answer with nothing.
    pub quantizations: Option<Vec<String>>,
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
    /// `effort` as the config has it, without the default: OpenAI gets
    /// `reasoning_effort` only when it is set, a model that does not think rejects it.
    pub effort_set: Option<String>,
    pub reasoning: bool,
    /// OpenRouter: the model can think. Then `reasoning = false` is sent as
    /// `enabled: false`: some models think by default.
    pub can_reason: bool,
    pub sort: Option<String>,
    pub ignore: Vec<String>,
    pub quantizations: Vec<String>,
    /// The model takes a strict JSON schema. When not, OpenRouter is asked for a JSON object.
    pub json_schema: bool,
}

/// The tag of the only theme when the config has no `[[themes]]`.
pub const DEFAULT_THEME: &str = "main";

/// One `[[themes]]` block. What is not set comes from `[prompts]`.
#[derive(Debug, Clone, Deserialize)]
pub struct ThemeConfig {
    /// One word that names the theme in commands: `/add dev @channel`.
    pub tag: String,
    /// Digest title, with `{{date}}` and `{{time}}`. Not set: `prompts.digest_title`.
    #[serde(default)]
    pub title: Option<String>,
    /// System prompt of the theme. Not set: `prompts.system`.
    #[serde(default)]
    pub system: Option<String>,
    /// Where the digest of this theme goes: `"bot"` or `"@channel"`. Not set:
    /// `telegram.destination`.
    #[serde(default)]
    pub destination: Option<String>,
}

/// A theme with everything one run needs: `prompts` already has the system prompt
/// and the title of this theme.
#[derive(Debug, Clone)]
pub struct Theme {
    pub tag: String,
    pub prompts: Prompts,
    /// `"bot"` (or `"me"`, for old configs), or `"@channel"`.
    pub destination: String,
}

impl Theme {
    /// The digest goes to the chat with the bot, where the owner can send commands.
    pub fn to_bot(&self) -> bool {
        matches!(self.destination.as_str(), "bot" | "me")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Prompts {
    /// The system prompt for themes that do not have their own.
    #[serde(default)]
    pub system: String,
    pub user_template: String,
    /// Instruction for the map step, when the input does not fit in one request.
    /// Not set — `summarize::DEFAULT_MAP_SYSTEM` is used.
    #[serde(default)]
    pub map_system: Option<String>,
    /// Digest title; `{{date}}` and `{{time}}` are in the time zone from `[schedule]`.
    #[serde(default = "default_digest_title")]
    pub digest_title: String,
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

        cfg.telegram.app.phone = normalize_phone(&cfg.telegram.app.phone);
        cfg.validate()?;
        Ok(cfg)
    }

    /// The themes in the order they run. Without `[[themes]]` it is one theme
    /// made from `[prompts]`.
    pub fn themes(&self) -> Vec<Theme> {
        if self.themes.is_empty() {
            return vec![Theme {
                tag: DEFAULT_THEME.into(),
                prompts: self.prompts.clone(),
                destination: self.telegram.destination.clone(),
            }];
        }
        self.themes
            .iter()
            .map(|t| {
                let mut prompts = self.prompts.clone();
                if let Some(system) = &t.system {
                    prompts.system = system.clone();
                }
                if let Some(title) = &t.title {
                    prompts.digest_title = title.clone();
                }
                Theme {
                    tag: t.tag.clone(),
                    prompts,
                    destination: t
                        .destination
                        .clone()
                        .unwrap_or_else(|| self.telegram.destination.clone()),
                }
            })
            .collect()
    }

    /// The theme with this tag; the case of the letters does not matter.
    pub fn theme(&self, tag: &str) -> Option<Theme> {
        self.themes()
            .into_iter()
            .find(|t| t.tag.eq_ignore_ascii_case(tag))
    }

    /// The provider API and its key. Fails with a clear message if the provider is
    /// unknown, not in the config, or has no key.
    pub fn provider_access(&self, provider_name: &str) -> Result<(ProviderKind, String)> {
        let kind = ProviderKind::from_name(provider_name).with_context(|| {
            format!("unknown provider {provider_name:?}, it must be one of {PROVIDERS:?}")
        })?;
        let provider = self
            .providers
            .get(provider_name)
            .with_context(|| format!("there is no [providers.{provider_name}]"))?;
        if provider.api_key.is_empty() {
            bail!(
                "providers.{provider_name}.api_key is not set (get it at {})",
                kind.key_url()
            );
        }
        Ok((kind, provider.api_key.clone()))
    }

    /// The provider and the model id in its API for `provider/model`. Only the first
    /// `/` splits: OpenRouter ids have their own (`openrouter/google/gemma-4-31b-it:free`).
    pub fn model_id(&self, key: &str) -> Result<(String, String)> {
        let (provider_name, model_name) = key.split_once('/').with_context(|| {
            format!("{key:?}: write the model as provider/model, for example anthropic/claude-haiku-5-5")
        })?;
        self.provider_access(provider_name)
            .with_context(|| key.to_string())?;
        let id = self.providers[provider_name]
            .models
            .get(model_name)
            .and_then(|m| m.id.clone())
            .unwrap_or_else(|| model_name.to_string());
        Ok((provider_name.to_string(), id))
    }

    /// A model with all its settings. Each setting comes from the first place that
    /// has it: the model table in the config, the provider list (`entry`, see
    /// `models.rs`), the provider table in the config, the defaults.
    pub fn model(&self, key: &str, entry: Option<&Entry>) -> Result<ModelSpec> {
        let (provider_name, id) = self.model_id(key)?;
        let (kind, api_key) = self.provider_access(&provider_name)?;
        let provider = &self.providers[&provider_name];
        let model_name = &key[provider_name.len() + 1..];
        let none = ModelSettings::default();
        let m = provider
            .models
            .get(model_name)
            .map_or(&none, |m| &m.settings);
        let p = &provider.settings;

        let max_tokens = m.max_tokens.unwrap_or_else(|| {
            let wanted = entry
                .and_then(|e| e.output_tokens)
                .map(|t| t.min(u64::from(kind.default_max_tokens())) as u32)
                .or(p.max_tokens)
                .unwrap_or_else(|| kind.default_max_tokens());
            // A small model (32k tokens for everything) with the default answer limit
            // would have almost no room for the input: every post would go to the
            // model alone. The answer gets at most half of the window.
            match entry.and_then(|e| e.input_tokens) {
                Some(window) => wanted.min(u32::try_from(window / 2).unwrap_or(u32::MAX).max(1)),
                None => wanted,
            }
        });
        let max_input_chars = m
            .max_input_chars
            .or_else(|| {
                let tokens = entry?.input_tokens?.saturating_sub(u64::from(max_tokens));
                Some((tokens * CHARS_PER_TOKEN_X10 / 10) as usize)
            })
            .or(p.max_input_chars)
            .unwrap_or_else(|| kind.default_max_input_chars());

        Ok(ModelSpec {
            key: key.to_string(),
            kind,
            api_key,
            api_url: provider
                .api_url
                .clone()
                .unwrap_or_else(|| kind.default_api_url().to_string()),
            id,
            max_tokens,
            max_input_chars,
            request_timeout_secs: m
                .request_timeout_secs
                .or(p.request_timeout_secs)
                .unwrap_or(600),
            effort: m
                .effort
                .clone()
                .or_else(|| p.effort.clone())
                .unwrap_or_else(|| "high".into()),
            effort_set: m.effort.clone().or_else(|| p.effort.clone()),
            // A model without reasoning would find no host with `require_parameters`.
            // A model that always thinks rejects "off" (400), so it is always on.
            reasoning: entry.is_some_and(|e| e.reasoning_mandatory)
                || (m.reasoning.or(p.reasoning).unwrap_or(false)
                    && entry.and_then(|e| e.reasoning).unwrap_or(true)),
            can_reason: entry.and_then(|e| e.reasoning).unwrap_or(false),
            // `sort = ""` means "not set", not a value for OpenRouter.
            sort: m
                .sort
                .clone()
                .or_else(|| p.sort.clone())
                .filter(|s| !s.trim().is_empty()),
            ignore: m
                .ignore
                .clone()
                .or_else(|| p.ignore.clone())
                .unwrap_or_default(),
            quantizations: m
                .quantizations
                .clone()
                .or_else(|| p.quantizations.clone())
                .unwrap_or_default(),
            json_schema: entry.and_then(|e| e.json_schema).unwrap_or(true),
        })
    }

    /// The models the config names, as `provider/model`: `model`, `fallback` and
    /// the model tables, each once.
    pub fn model_keys(&self) -> Vec<String> {
        let tables = self
            .providers
            .iter()
            .flat_map(|(provider, p)| p.models.keys().map(move |m| format!("{provider}/{m}")));
        let mut keys: Vec<String> = Vec::new();
        for key in std::iter::once(self.model.clone())
            .chain(self.fallback.iter().cloned())
            .chain(tables)
        {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys
    }

    /// We catch empty required fields here, not on the first network call:
    /// otherwise the error shows up in the middle of a night run as a 401.
    fn validate(&self) -> Result<()> {
        let app = &self.telegram.app;
        if app.api_id == 0 || app.api_hash.is_empty() {
            bail!("telegram.app.api_id / telegram.app.api_hash are not set (get them at https://my.telegram.org)");
        }
        if app.phone.is_empty() {
            bail!("telegram.app.phone is not set");
        }
        for name in self.providers.keys() {
            if ProviderKind::from_name(name).is_none() {
                bail!("[providers.{name}]: unknown provider, it must be one of {PROVIDERS:?}");
            }
        }
        // Only the form and the key here: whether the provider has the model is
        // checked against its list (`models.rs`).
        self.model(&self.model, None).context("model")?;
        for key in &self.fallback {
            self.model(key, None).context("fallback")?;
        }
        if self.schedule.timezone.parse::<chrono_tz::Tz>().is_err() {
            bail!(
                "schedule.timezone {:?} is unknown, an IANA name like America/New_York is needed",
                self.schedule.timezone
            );
        }
        // The tag is the key of the theme in the database, so its form is strict.
        let mut tags: Vec<&str> = Vec::new();
        for theme in &self.themes {
            let tag = theme.tag.as_str();
            let word = !tag.is_empty()
                && tag
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
            if !word {
                bail!("themes: tag {tag:?} must be one word of small letters, digits, _ or -");
            }
            if tags.contains(&tag) {
                bail!("themes: tag {tag:?} is used twice");
            }
            tags.push(tag);
        }
        for theme in self.themes() {
            if theme.prompts.system.trim().is_empty() {
                bail!(
                    "theme {:?} has no system prompt: set `system` in its [[themes]] block or in [prompts]",
                    theme.tag
                );
            }
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
