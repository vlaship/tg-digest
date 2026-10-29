//! Загрузка `config.toml`: секреты, расписание, промпты, лимиты.
//!
//! Секреты живут в TOML, но любая из переменных окружения `TG_API_ID`,
//! `TG_API_HASH`, `TG_PHONE`, `ANTHROPIC_API_KEY` перекрывает значение из файла —
//! так один и тот же конфиг переносится на другую машину без правок.
//!
//! Списка каналов здесь больше нет: правда о каналах — в папке Telegram и в
//! базе (`db.rs`). `[[channels]]` остались только как seed для пустой базы.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub schedule: Schedule,
    pub telegram: Telegram,
    pub anthropic: Anthropic,
    pub prompts: Prompts,
    /// Файл базы. В контейнере — в volume `/data`, рядом с сессией.
    #[serde(default = "default_db_file")]
    pub db_file: String,
    /// Seed: используется только если таблица каналов пуста.
    #[serde(default)]
    pub channels: Vec<SeedChannel>,
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
    /// Имя папки (dialog filter) в аккаунте — источник списка каналов.
    /// Не задано → каналами управляют только команды `/add` и `/rm`.
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default = "default_lookback_hours")]
    pub lookback_hours: i64,
    /// Предохранитель от чтения всей истории, **не** отсечка дайджеста: канал,
    /// упёршийся в лимит, исключается из прогона целиком и курсор не двигает.
    #[serde(default = "default_max_posts")]
    pub max_posts_per_channel: usize,
    #[serde(default = "default_min_post_chars")]
    pub min_post_chars: usize,
    #[serde(default = "default_pause_ms")]
    pub pause_between_channels_ms: u64,
    /// Сколько часов демон сам добивает недоставленный дайджест. За пределами
    /// окна повтор — только вручную (`/resume`), см. PLAN.md 9.3.
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
    /// Инструкция для шага map, когда вход не влезает в один запрос.
    /// Не задана — берётся `summarize::DEFAULT_MAP_SYSTEM`.
    #[serde(default)]
    pub map_system: Option<String>,
    /// Заголовок дайджеста; `{{date}}` — дата в таймзоне из `[schedule]`.
    #[serde(default = "default_digest_title")]
    pub digest_title: String,
    #[serde(default = "default_tldr_title")]
    pub tldr_title: String,
}

/// Канал из конфига. Нужен только чтобы наполнить пустую базу при первом
/// запуске: дальше список живёт в папке Telegram и в базе.
#[derive(Debug, Clone, Deserialize)]
pub struct SeedChannel {
    #[serde(default)]
    pub username: Option<String>,
    /// Приватный канал: `-100…` или «голый» id. Работает без username.
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| {
            format!(
                "не удалось прочитать {}. Скопируйте config.example.toml в config.toml и заполните",
                path.display()
            )
        })?;

        let mut cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("{} — некорректный TOML", path.display()))?;

        cfg.apply_env_overrides();
        cfg.telegram.phone = normalize_phone(&cfg.telegram.phone);
        cfg.validate()?;
        Ok(cfg)
    }

    fn apply_env_overrides(&mut self) {
        if let Some(v) = env("TG_API_ID") {
            match v.parse() {
                Ok(id) => self.telegram.api_id = id,
                Err(_) => tracing::warn!("TG_API_ID={v} — не число, значение из конфига оставлено"),
            }
        }
        if let Some(v) = env("TG_API_HASH") {
            self.telegram.api_hash = v;
        }
        if let Some(v) = env("TG_PHONE") {
            self.telegram.phone = v;
        }
        if let Some(v) = env("ANTHROPIC_API_KEY") {
            self.anthropic.api_key = v;
        }
    }

    /// Ловим пустые обязательные поля здесь, а не на первом сетевом вызове:
    /// иначе ошибка вылезет посреди ночного прогона в виде 401.
    fn validate(&self) -> Result<()> {
        if self.telegram.api_id == 0 || self.telegram.api_hash.is_empty() {
            bail!("не заданы telegram.api_id / telegram.api_hash (получить на https://my.telegram.org)");
        }
        if self.telegram.phone.is_empty() {
            bail!("не задан telegram.phone");
        }
        if self.anthropic.api_key.is_empty() {
            bail!("не задан anthropic.api_key (или переменная окружения ANTHROPIC_API_KEY)");
        }
        if self.telegram.lookback_hours <= 0 {
            bail!("telegram.lookback_hours должен быть больше нуля");
        }
        if self.telegram.max_posts_per_channel == 0 {
            bail!("telegram.max_posts_per_channel должен быть больше нуля");
        }
        if self.telegram.publish_retry_window_hours < 0 {
            bail!("telegram.publish_retry_window_hours не может быть отрицательным");
        }
        if self.telegram.folder.as_deref().map(str::trim) == Some("") {
            bail!("telegram.folder задан пустой строкой — уберите ключ или впишите имя папки");
        }
        for channel in &self.channels {
            if channel.username.is_none() && channel.id.is_none() {
                bail!("в [[channels]] есть запись без username и без id");
            }
        }
        Ok(())
    }

    /// Каналы-seed, которые надо занести в пустую базу.
    pub fn seed_channels(&self) -> Vec<&SeedChannel> {
        self.channels.iter().filter(|c| c.enabled).collect()
    }
}

/// Пустая переменная окружения — это «не задано», а не «сотри ключ из конфига».
/// Оркестраторы охотно подставляют пустую строку вместо незаданной переменной,
/// и без этой проверки конфиг с секретами ломался бы на пустом `.env`.
fn env(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    (!value.trim().is_empty()).then_some(value)
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
    "📰 Дайджест за {{date}}".into()
}
fn default_tldr_title() -> String {
    "Коротко".into()
}
fn default_true() -> bool {
    true
}

/// Telegram ждёт номер цифрами: дефисы, пробелы и скобки из конфига вычищаем
/// сами, иначе `login` падает с PHONE_NUMBER_INVALID на ровном месте.
/// Ведущий `+` сохраняем, если он был.
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
    fn minimal_config_is_valid_without_any_channels() {
        let cfg = parse(MINIMAL).expect("каналы живут в базе, конфиг без них корректен");
        assert_eq!(cfg.telegram.max_posts_per_channel, 1000);
        assert_eq!(cfg.telegram.publish_retry_window_hours, 2);
        assert!(cfg.telegram.folder.is_none());
        assert_eq!(cfg.db_file, "/data/digest.db");
    }

    #[test]
    fn seed_channel_needs_a_username_or_an_id() {
        let raw = format!("{MINIMAL}\n[[channels]]\ntitle = \"без адреса\"\n");
        assert!(parse(&raw).is_err());

        let raw = format!("{MINIMAL}\n[[channels]]\nid = -1001234567890\n");
        assert_eq!(parse(&raw).unwrap().seed_channels().len(), 1);
    }

    /// Пример конфига — часть документации: он обязан разбираться теми же
    /// структурами, что и рабочий, иначе разъедется незаметно.
    #[test]
    fn example_config_parses() {
        let cfg: Config = toml::from_str(include_str!("../config.example.toml"))
            .expect("config.example.toml должен разбираться");

        assert_eq!(cfg.telegram.folder.as_deref(), Some("Digest"));
        assert_eq!(cfg.telegram.publish_retry_window_hours, 2);
        assert_eq!(cfg.telegram.max_posts_per_channel, 1000);
        assert_eq!(cfg.db_file, "/data/digest.db");
        assert!(cfg.prompts.digest_title.contains("{{date}}"));
        assert!(cfg.prompts.user_template.contains("{{posts}}"));
    }

    #[test]
    fn empty_folder_name_is_rejected() {
        let raw = MINIMAL.replace("phone =", "folder = \"\"\nphone =");
        assert!(parse(&raw).is_err());
    }

    /// Переменные окружения общие на процесс, поэтому проверка одна и идёт
    /// последовательно внутри теста.
    #[test]
    fn empty_env_override_does_not_erase_a_configured_secret() {
        let mut cfg: Config = toml::from_str(MINIMAL).unwrap();

        std::env::set_var("TG_API_HASH", "");
        std::env::set_var("ANTHROPIC_API_KEY", "   ");
        cfg.apply_env_overrides();
        assert_eq!(cfg.telegram.api_hash, "hash");
        assert_eq!(cfg.anthropic.api_key, "key");

        std::env::set_var("TG_API_HASH", "from-env");
        cfg.apply_env_overrides();
        assert_eq!(cfg.telegram.api_hash, "from-env");

        std::env::remove_var("TG_API_HASH");
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
}
