//! Loading `config.toml`: secrets, schedule, prompts, limits — everything comes from
//! this one file (in a container: `/data/config.toml`).
//!
//! There is no channel list here: it lives only in the database (`db.rs`) and is
//! managed with `/add` and `/rm` in Saved Messages.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub schedule: Schedule,
    pub telegram: Telegram,
    pub anthropic: Anthropic,
    pub prompts: Prompts,
    /// Database file. In a container it is in the `/data` volume, next to the session.
    #[serde(default = "default_db_file")]
    pub db_file: String,
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

#[derive(Debug, Deserialize)]
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

    /// We catch empty required fields here, not on the first network call:
    /// otherwise the error shows up in the middle of a night run as a 401.
    fn validate(&self) -> Result<()> {
        if self.telegram.api_id == 0 || self.telegram.api_hash.is_empty() {
            bail!("telegram.api_id / telegram.api_hash are not set (get them at https://my.telegram.org)");
        }
        if self.telegram.phone.is_empty() {
            bail!("telegram.phone is not set");
        }
        if self.anthropic.api_key.is_empty() {
            bail!("anthropic.api_key is not set");
        }
        if self.telegram.lookback_hours <= 0 {
            bail!("telegram.lookback_hours must be greater than zero");
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
    "me".into()
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
fn default_pause_ms() -> u64 {
    1500
}
fn default_publish_retry_window_hours() -> i64 {
    2
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

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [schedule]
        cron = "0 9 * * *"

        [telegram]
        api_id = 1
        api_hash = "hash"
        phone = "+15550001122"

        [anthropic]
        api_key = "key"

        [prompts]
        system = "s"
        user_template = "u"
    "#;

    fn parse(raw: &str) -> Result<Config> {
        let mut cfg: Config = toml::from_str(raw)?;
        cfg.telegram.phone = normalize_phone(&cfg.telegram.phone);
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn strips_separators_and_keeps_plus() {
        assert_eq!(normalize_phone(" +1-555-000-11-22 "), "+15550001122");
        assert_eq!(normalize_phone("+1 (555) 000-1122"), "+15550001122");
    }

    #[test]
    fn keeps_bare_number_without_plus() {
        assert_eq!(normalize_phone("15550001122"), "15550001122");
    }

    #[test]
    fn empty_stays_empty_so_validation_catches_it() {
        assert_eq!(normalize_phone(""), "");
        assert_eq!(normalize_phone("+"), "");
    }

    #[test]
    fn minimal_config_is_valid() {
        let cfg = parse(MINIMAL).expect("channels live in the database, the config has none");
        assert_eq!(cfg.telegram.max_posts_per_channel, 1000);
        assert_eq!(cfg.telegram.publish_retry_window_hours, 2);
        assert_eq!(cfg.db_file, "/data/digest.db");
        assert_eq!(cfg.telegram.session_file, "/data/tg.session");
    }

    /// Old configs may still have `folder` and `[[channels]]`: they are ignored,
    /// so an existing data/config.toml keeps working after the update.
    #[test]
    fn old_folder_and_channel_keys_are_ignored() {
        let raw = MINIMAL.replace("phone =", "folder = \"Digest\"\nphone =")
            + "\n[[channels]]\nusername = \"news\"\n";
        assert!(parse(&raw).is_ok());
    }

    /// The example config is part of the docs: it must be parsed by the same
    /// structures as the real one, or they drift apart unnoticed.
    #[test]
    fn example_config_parses() {
        let cfg: Config = toml::from_str(include_str!("../config.example.toml"))
            .expect("config.example.toml must parse");

        assert_eq!(cfg.telegram.publish_retry_window_hours, 2);
        assert_eq!(cfg.telegram.max_posts_per_channel, 1000);
        assert_eq!(cfg.db_file, "/data/digest.db");
        assert!(cfg.prompts.digest_title.contains("{{date}}"));
        assert!(cfg.prompts.user_template.contains("{{posts}}"));
    }
}
