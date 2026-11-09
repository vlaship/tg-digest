//! The model catalog: the models each provider has, read from its own API.
//!
//! No model id is written in the code. The config names the models (`model`,
//! `fallback`), and their limits (input and output tokens, strict JSON schema)
//! come from this catalog. A `[providers.<p>.models."<id>"]` table is needed only to
//! change something by hand.
//!
//! The lists are kept in the `settings` table, so a restart does not ask the APIs
//! again, and a run still works when a list cannot be read.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use crate::config::{Config, ModelSpec, ProviderKind};
use crate::db::Db;

/// How long a list is good. New models do not appear every hour.
const MAX_AGE_HOURS: i64 = 24;
/// Key prefix in the `settings` table: `models:gemini`.
const SETTING_PREFIX: &str = "models:";

/// One model as its provider describes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    /// The input limit (for OpenRouter and Anthropic: the whole context).
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// OpenRouter: the model takes a strict JSON schema (`structured_outputs`).
    pub json_schema: Option<bool>,
    /// OpenRouter: the model can think before it answers.
    pub reasoning: Option<bool>,
    /// OpenRouter: the model always thinks; "off" gets 400. Lists saved before
    /// this field was added do not have it.
    #[serde(default)]
    pub reasoning_mandatory: bool,
    /// The model costs nothing (OpenRouter `:free`).
    pub free: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Listing {
    fetched: DateTime<Utc>,
    models: Vec<Entry>,
}

pub struct Catalog {
    cfg: Arc<Config>,
    db: Arc<Db>,
    http: reqwest::Client,
    lists: RwLock<BTreeMap<String, Listing>>,
}

impl Catalog {
    pub fn new(cfg: Arc<Config>, db: Arc<Db>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        Self {
            cfg,
            db,
            http,
            lists: RwLock::new(BTreeMap::new()),
        }
    }

    /// The models of a provider. An old list is read again; if that fails, the old
    /// list is used, and only with no list at all this is an error.
    pub async fn list(&self, provider: &str) -> Result<Vec<Entry>> {
        if let Some(listing) = self.cached(provider).await {
            if Utc::now() - listing.fetched < ChronoDuration::hours(MAX_AGE_HOURS) {
                return Ok(listing.models);
            }
            return match self.refresh(provider).await {
                Ok(models) => Ok(models),
                Err(e) => {
                    tracing::warn!(
                        provider,
                        error = %format!("{e:#}"),
                        "could not read the model list, using the old one"
                    );
                    Ok(listing.models)
                }
            };
        }
        self.refresh(provider).await
    }

    /// Read the list from the provider API now and keep it.
    pub async fn refresh(&self, provider: &str) -> Result<Vec<Entry>> {
        let (kind, api_key) = self.cfg.provider_access(provider)?;
        let models = fetch(&self.http, kind, &api_key)
            .await
            .with_context(|| format!("could not read the {} model list", kind.api_name()))?;
        tracing::info!(provider, models = models.len(), "model list read");
        let listing = Listing {
            fetched: Utc::now(),
            models: models.clone(),
        };
        match serde_json::to_string(&listing) {
            Ok(json) => {
                let key = format!("{SETTING_PREFIX}{provider}");
                if let Err(e) = self.db.set_setting(&key, &json).await {
                    tracing::warn!(error = %format!("{e:#}"), "could not save the model list");
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not save the model list"),
        }
        self.lists
            .write()
            .await
            .insert(provider.to_string(), listing);
        Ok(models)
    }

    async fn cached(&self, provider: &str) -> Option<Listing> {
        if let Some(listing) = self.lists.read().await.get(provider) {
            return Some(listing.clone());
        }
        let json = self
            .db
            .setting(&format!("{SETTING_PREFIX}{provider}"))
            .await
            .ok()??;
        let listing: Listing = serde_json::from_str(&json).ok()?;
        self.lists
            .write()
            .await
            .insert(provider.to_string(), listing.clone());
        Some(listing)
    }

    /// A model with all its settings: the config first, then the catalog, then the
    /// provider defaults.
    ///
    /// A model that the provider does not have is an error at once, not a 404 in the
    /// middle of a run. If the list cannot be read at all, the model is used as it is.
    pub async fn spec(&self, key: &str) -> Result<ModelSpec> {
        let (provider, id) = self.cfg.model_id(key)?;
        let entry = match self.list(&provider).await {
            Ok(models) => match models.into_iter().find(|m| m.id == id) {
                Some(entry) => Some(entry),
                None => bail!("{provider} has no model {id:?}: see /models {provider}"),
            },
            Err(e) => {
                tracing::warn!(
                    model = key,
                    error = %format!("{e:#}"),
                    "no model list, the limits come from the config and the defaults"
                );
                None
            }
        };
        self.cfg.model(key, entry.as_ref())
    }
}

/// The URL of the model list. Not from `api_url`: that one is for the requests.
fn list_url(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "https://api.anthropic.com/v1/models",
        ProviderKind::Gemini => "https://generativelanguage.googleapis.com/v1beta/models",
        ProviderKind::OpenRouter => "https://openrouter.ai/api/v1/models",
        ProviderKind::OpenAI => "https://api.openai.com/v1/models",
    }
}

async fn fetch(http: &reqwest::Client, kind: ProviderKind, api_key: &str) -> Result<Vec<Entry>> {
    let url = list_url(kind);
    let mut models = Vec::new();
    // Anthropic and Gemini give the list in pages.
    let mut page: Option<String> = None;
    loop {
        let request = match kind {
            ProviderKind::Anthropic => {
                let mut query = vec![("limit", "1000".to_string())];
                if let Some(after) = &page {
                    query.push(("after_id", after.clone()));
                }
                http.get(url)
                    .query(&query)
                    .header("x-api-key", api_key)
                    .header("anthropic-version", crate::summarize::ANTHROPIC_VERSION)
            }
            ProviderKind::Gemini => {
                let mut query = vec![("pageSize", "1000".to_string())];
                if let Some(token) = &page {
                    query.push(("pageToken", token.clone()));
                }
                http.get(url)
                    .query(&query)
                    .header("x-goog-api-key", api_key)
            }
            ProviderKind::OpenRouter | ProviderKind::OpenAI => http.get(url).bearer_auth(api_key),
        };
        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            let short: String = text.chars().take(300).collect();
            bail!("HTTP {status}: {short}");
        }
        let body: Value = serde_json::from_str(&text).context("the answer is not JSON")?;

        page = match kind {
            ProviderKind::Anthropic => {
                models.extend(items(&body, "data").filter_map(anthropic_entry));
                match body["has_more"].as_bool() {
                    Some(true) => body["last_id"].as_str().map(str::to_string),
                    _ => None,
                }
            }
            ProviderKind::Gemini => {
                models.extend(items(&body, "models").filter_map(gemini_entry));
                body["nextPageToken"]
                    .as_str()
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
            }
            ProviderKind::OpenRouter => {
                models.extend(items(&body, "data").filter_map(openrouter_entry));
                None
            }
            ProviderKind::OpenAI => {
                models.extend(items(&body, "data").filter_map(openai_entry));
                None
            }
        };
        if page.is_none() {
            break;
        }
    }
    if models.is_empty() {
        bail!("the list is empty");
    }
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

fn items<'a>(body: &'a Value, field: &str) -> impl Iterator<Item = &'a Value> {
    body[field].as_array().into_iter().flatten()
}

fn anthropic_entry(m: &Value) -> Option<Entry> {
    Some(Entry {
        id: m["id"].as_str()?.to_string(),
        input_tokens: m["max_input_tokens"].as_u64(),
        output_tokens: m["max_tokens"].as_u64(),
        json_schema: None,
        reasoning: None,
        reasoning_mandatory: false,
        free: false,
    })
}

/// Only models that can write text (`generateContent`): the list also has
/// embedding and image models.
fn gemini_entry(m: &Value) -> Option<Entry> {
    let writes = m["supportedGenerationMethods"]
        .as_array()?
        .iter()
        .any(|x| x == "generateContent");
    if !writes {
        return None;
    }
    Some(Entry {
        id: m["name"]
            .as_str()?
            .trim_start_matches("models/")
            .to_string(),
        input_tokens: m["inputTokenLimit"].as_u64(),
        output_tokens: m["outputTokenLimit"].as_u64(),
        json_schema: None,
        reasoning: None,
        reasoning_mandatory: false,
        free: false,
    })
}

/// The OpenAI list has only ids, no limits, and also has models that cannot write
/// a chat answer (embeddings, speech, images): we keep the chat ones.
fn openai_entry(m: &Value) -> Option<Entry> {
    let id = m["id"].as_str()?;
    let chat = id.starts_with("gpt-")
        || id.starts_with("chatgpt-")
        || (id.starts_with('o') && id[1..].starts_with(|c: char| c.is_ascii_digit()));
    let other = [
        "audio",
        "realtime",
        "transcribe",
        "tts",
        "image",
        "search",
        "embedding",
    ];
    if !chat || other.iter().any(|o| id.contains(o)) {
        return None;
    }
    Some(Entry {
        id: id.to_string(),
        input_tokens: None,
        output_tokens: None,
        json_schema: None,
        reasoning: None,
        reasoning_mandatory: false,
        free: false,
    })
}

fn openrouter_entry(m: &Value) -> Option<Entry> {
    let params: Vec<&str> = m["supported_parameters"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    // Without `response_format` the answer may be any text, not JSON.
    if !params.contains(&"response_format") && !params.contains(&"structured_outputs") {
        return None;
    }
    let id = m["id"].as_str()?.to_string();
    let price = |field: &str| m["pricing"][field].as_str().unwrap_or("1");
    let free = id.ends_with(":free") || (price("prompt") == "0" && price("completion") == "0");
    Some(Entry {
        // The model may allow more than the host that answers (`top_provider`).
        input_tokens: [
            m["context_length"].as_u64(),
            m["top_provider"]["context_length"].as_u64(),
        ]
        .into_iter()
        .flatten()
        .min(),
        output_tokens: m["top_provider"]["max_completion_tokens"].as_u64(),
        json_schema: Some(params.contains(&"structured_outputs")),
        reasoning: Some(params.contains(&"reasoning")),
        reasoning_mandatory: m["reasoning"]["mandatory"].as_bool().unwrap_or(false),
        free,
        id,
    })
}
