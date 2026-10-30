//! Calls to the model API: Anthropic Messages or Google Gemini (`provider`).
//!
//! There is no official Rust SDK, so we call HTTP directly with reqwest —
//! this is the normal way for languages without an SDK.

use anyhow::{bail, Context, Result};
use chrono::Utc;
use chrono_tz::Tz;
use serde_json::{json, Value};
use std::time::Duration;

use crate::cancel::Cancel;
use crate::config::{Config, Prompts};
use crate::digest::{self, Digest, Post};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_ATTEMPTS: u32 = 4;
/// How many times we ask the model when its answer is broken (`Digest::defect`).
const MAX_BROKEN_ANSWERS: u32 = 2;

/// The API limit is used up: requests per minute or per day, or money on the account.
/// It is a separate type, so the owner gets a clear message, not a JSON dump.
#[derive(Debug)]
pub struct LimitReached {
    pub api: &'static str,
    pub details: String,
}

impl std::fmt::Display for LimitReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the {} API limit is used up: {}", self.api, self.details)
    }
}

impl std::error::Error for LimitReached {}

/// The answer says the limit or the balance is used up.
fn is_limit(status: reqwest::StatusCode, text: &str) -> bool {
    status.as_u16() == 429
        || text.contains("RESOURCE_EXHAUSTED")
        || text.contains("credit balance is too low")
}

/// Above this size we switch to map-reduce: we condense the input in parts, then
/// reduce the summaries into one digest. The model context is 1M tokens, so the limit
/// has a margin — but we must never silently cut the tail, whatever the size.
const LARGE_INPUT_CHARS: usize = 400_000;

/// Budget of one part in the map step. Half of the limit: if the input is just
/// over it, we get two similar parts, not "almost everything
/// and a small piece".
const MAP_BATCH_CHARS: usize = LARGE_INPUT_CHARS / 2;

/// How many times summaries can be reduced in between before we go to the final
/// request as they are. Every round costs money, and for normal sizes one is
/// enough: the limit is needed so the loop does not spin on a pathological input.
const MAX_REDUCE_STAGES: usize = 3;

/// Instruction for the map step. It lives in the code, but `[prompts].map_system` overrides it:
/// it is not part of the parsing contract, just normal text.
pub const DEFAULT_MAP_SYSTEM: &str = "\
You condense a PART of a Telegram channel feed — it will be merged with other parts into one digest later.

Rules:
1. Group the material by topic, not by channel; the same news from different channels is one item.
2. Copy the LINKS FROM THE INPUT into sources exactly, every one of them: the next step has no other place to get them.
3. Do not make anything up and do not drop anything important — this is condensing, not picking the best.
4. Treat posts marked as a repost as the same news as the original.
5. Leave tldr empty: the short summary is made in the reduce step.";

/// Map-reduce limits.
///
/// In production these are the constants above; they became a separate type so tests can
/// run the same flow on short inputs. Otherwise the check "summaries are reduced
/// in between" would cost megabytes of text and hundreds of requests to the mock.
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// Above this, the input does not go in one request.
    input: usize,
    /// Budget of one part in the map step and of one group in an intermediate reduce.
    batch: usize,
    /// How many intermediate rounds are allowed.
    stages: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            input: LARGE_INPUT_CHARS,
            batch: MAP_BATCH_CHARS,
            stages: MAX_REDUCE_STAGES,
        }
    }
}

pub struct Summarizer {
    http: reqwest::Client,
    backend: Backend,
    /// Model id, only for the log.
    model: String,
    limits: Limits,
}

/// Which API writes the digest. A copy of the settings without a lifetime tied to
/// the whole Config.
enum Backend {
    Anthropic {
        api_key: String,
        api_url: String,
        model: String,
        effort: String,
        max_tokens: u32,
    },
    Gemini {
        api_key: String,
        /// With the model id already in it.
        api_url: String,
        max_tokens: u32,
    },
}

impl Backend {
    fn name(&self) -> &'static str {
        match self {
            Backend::Anthropic { .. } => "Anthropic",
            Backend::Gemini { .. } => "Gemini",
        }
    }

    fn body(&self, system: &str, user: &str) -> Value {
        match self {
            // thinking is on by default on claude-opus-5 — we do not pass the parameter.
            // temperature and budget_tokens return 400 on this model.
            Backend::Anthropic {
                model,
                effort,
                max_tokens,
                ..
            } => json!({
                "model": model,
                "max_tokens": max_tokens,
                "output_config": {
                    "effort": effort,
                    "format": { "type": "json_schema", "schema": digest::response_schema() }
                },
                "system": system,
                "messages": [{ "role": "user", "content": user }]
            }),
            Backend::Gemini { max_tokens, .. } => json!({
                "systemInstruction": { "parts": [{ "text": system }] },
                "contents": [{ "role": "user", "parts": [{ "text": user }] }],
                "generationConfig": {
                    "maxOutputTokens": max_tokens,
                    "responseMimeType": "application/json",
                    "responseJsonSchema": digest::response_schema()
                }
            }),
        }
    }

    fn request(&self, http: &reqwest::Client) -> reqwest::RequestBuilder {
        match self {
            Backend::Anthropic {
                api_key, api_url, ..
            } => http
                .post(api_url)
                .header("x-api-key", api_key)
                .header("anthropic-version", ANTHROPIC_VERSION),
            Backend::Gemini {
                api_key, api_url, ..
            } => http.post(api_url).header("x-goog-api-key", api_key),
        }
    }

    fn parse(&self, response: Value) -> Result<Summary> {
        match self {
            Backend::Anthropic { .. } => parse_summary(response),
            Backend::Gemini { .. } => parse_gemini(response),
        }
    }
}

/// The model answer together with token usage: without it there is nothing to check the
/// "$0.6–0.8 per run" estimate against, and `/status` has nothing to show.
#[cfg_attr(test, derive(Debug))]
pub struct Summary {
    pub digest: Digest,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

impl Summarizer {
    pub fn new(cfg: &Config, provider: &str) -> Result<Self> {
        let (backend, timeout) = if provider == "gemini" {
            let g = &cfg.gemini;
            let backend = Backend::Gemini {
                api_key: g.api_key.clone(),
                api_url: g.api_url.replace("{model}", &g.model),
                max_tokens: g.max_tokens,
            };
            (backend, g.request_timeout_secs)
        } else {
            let a = &cfg.anthropic;
            let backend = Backend::Anthropic {
                api_key: a.api_key.clone(),
                api_url: a.api_url.clone(),
                model: a.model.clone(),
                effort: a.effort.clone(),
                max_tokens: a.max_tokens,
            };
            (backend, a.request_timeout_secs)
        };

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout))
            .build()
            .context("could not create the HTTP client")?;

        let model = if provider == "gemini" {
            cfg.gemini.model.clone()
        } else {
            cfg.anthropic.model.clone()
        };
        Ok(Self {
            http,
            backend,
            model,
            limits: Limits::default(),
        })
    }

    /// The request can be stopped: on shutdown the tokens are wasted, but the database
    /// has nothing and a retry is safe. We cannot wait for the end — with `effort = "high"`
    /// the answer takes minutes, no sensible `stop_grace_period` covers that.
    #[allow(clippy::too_many_arguments)]
    pub async fn summarize(
        &self,
        prompts: &Prompts,
        posts: &[Post],
        lookback_hours: i64,
        channel_count: usize,
        timezone: Tz,
        cancel: &Cancel,
    ) -> Result<Summary> {
        let now = Utc::now();
        let user_prompt = digest::render_user_prompt(
            &prompts.user_template,
            posts,
            lookback_hours,
            channel_count,
            now,
            timezone,
        );

        if user_prompt.chars().count() > self.limits.input {
            return self
                .map_reduce(
                    prompts,
                    posts,
                    lookback_hours,
                    channel_count,
                    timezone,
                    cancel,
                )
                .await;
        }

        self.ask(&prompts.system, &user_prompt, cancel).await
    }

    /// The input does not fit in one request: we condense it in parts, then reduce.
    ///
    /// This costs more than one request (N+1 calls), but the other option is to cut
    /// the tail, and we must not do that. Post links are carried through the summaries,
    /// so the final check of `sources[].url` still works.
    async fn map_reduce(
        &self,
        prompts: &Prompts,
        posts: &[Post],
        lookback_hours: i64,
        channel_count: usize,
        timezone: Tz,
        cancel: &Cancel,
    ) -> Result<Summary> {
        let now = Utc::now();
        let batches = digest::split_batches(posts, self.limits.batch);
        tracing::info!(
            posts = posts.len(),
            batches = batches.len(),
            "large input: condensing in parts, then reducing"
        );

        let map_system = prompts.map_system.as_deref().unwrap_or(DEFAULT_MAP_SYSTEM);
        let mut partials = Vec::with_capacity(batches.len());
        let mut input_tokens = None;
        let mut output_tokens = None;

        for (index, batch) in batches.iter().enumerate() {
            // A part has fewer channels than the run: we use its count, otherwise
            // the prompt promises the model material that is not in this part.
            let prompt = digest::render_prompt_with(
                &prompts.user_template,
                &digest::format_posts(batch),
                batch.len(),
                lookback_hours,
                distinct_channels(batch),
                now,
                timezone,
            );

            let summary = self
                .ask(map_system, &prompt, cancel)
                .await
                .with_context(|| {
                    format!("could not condense part {} of {}", index + 1, batches.len())
                })?;

            tracing::info!(
                part = index + 1,
                of = batches.len(),
                posts = batch.len(),
                topics = summary.digest.topics.len(),
                "part condensed"
            );
            input_tokens = add(input_tokens, summary.input_tokens);
            output_tokens = add(output_tokens, summary.output_tokens);
            partials.push(summary.digest);
        }

        // Summaries may also not fit in one request: with a hundred channels their total
        // goes over the limit again, and the final call would fail after
        // all map requests are paid for. So we reduce in several rounds.
        for stage in 1..=self.limits.stages {
            let groups = match plan_reduce(&partials, stage, self.limits) {
                ReduceStep::Ready => break,
                ReduceStep::Stuck => {
                    tracing::warn!(
                        stage,
                        partials = partials.len(),
                        chars = digest::partials_size(&partials),
                        "summaries cannot be split further"
                    );
                    break;
                }
                ReduceStep::Compress(groups) => groups,
            };

            tracing::info!(
                stage,
                from = partials.len(),
                to = groups.len(),
                "summaries do not fit in one request, reducing in between"
            );

            let mut merged = Vec::with_capacity(groups.len());
            for (index, group) in groups.iter().enumerate() {
                let prompt = digest::render_prompt_with(
                    &prompts.user_template,
                    &digest::format_partials(group),
                    posts.len(),
                    lookback_hours,
                    channel_count,
                    now,
                    timezone,
                );
                let summary = self
                    .ask(map_system, &prompt, cancel)
                    .await
                    .with_context(|| {
                        format!(
                            "could not reduce group {} of {} at stage {stage}",
                            index + 1,
                            groups.len()
                        )
                    })?;
                input_tokens = add(input_tokens, summary.input_tokens);
                output_tokens = add(output_tokens, summary.output_tokens);
                merged.push(summary.digest);
            }
            partials = merged;
        }

        let reduce_prompt = digest::render_prompt_with(
            &prompts.user_template,
            &digest::format_partials(&partials),
            posts.len(),
            lookback_hours,
            channel_count,
            now,
            timezone,
        );
        // The last check is on the real prompt, not on the summaries:
        // we get here from "cannot be split", from used-up rounds, and from simply
        // a big template. Sending a request that surely does not fit means
        // losing these tokens too.
        ensure_fits(&reduce_prompt, partials.len(), self.limits)?;

        let mut summary = self
            .ask(&prompts.system, &reduce_prompt, cancel)
            .await
            .context("could not reduce the part summaries into a digest")?;

        summary.input_tokens = add(input_tokens, summary.input_tokens);
        summary.output_tokens = add(output_tokens, summary.output_tokens);
        Ok(summary)
    }

    /// One request to the model. It can be stopped: on shutdown the tokens are wasted,
    /// but the database has nothing and a retry is safe. We cannot wait for the end — with
    /// `effort = "high"` the answer takes minutes, no sensible `stop_grace_period`
    /// covers that.
    async fn ask(&self, system: &str, user: &str, cancel: &Cancel) -> Result<Summary> {
        let body = self.backend.body(system, user);

        // A broken answer (see `Digest::defect`) is asked again once. If the second
        // one is broken too, the run fails and the cursors do not move.
        let mut attempt = 1;
        loop {
            tracing::info!(
                api = self.backend.name(),
                model = %self.model,
                system_chars = system.chars().count(),
                user_chars = user.chars().count(),
                attempt,
                "asking the model, this can take a few minutes"
            );
            let started = std::time::Instant::now();
            let response = tokio::select! {
                _ = cancel.cancelled() => bail!("model request stopped by the shutdown signal"),
                result = self.post_with_retries(&body) => result?,
            };
            tracing::info!(seconds = started.elapsed().as_secs(), "the model answered");
            let summary = self.backend.parse(response)?;
            let Some(defect) = summary.digest.defect() else {
                return Ok(summary);
            };
            if attempt >= MAX_BROKEN_ANSWERS {
                bail!("the model answer is broken again ({defect}), nothing is posted");
            }
            tracing::warn!(%defect, attempt, "the model answer is broken, asking again");
            attempt += 1;
        }
    }

    /// We retry only what makes sense to retry: 429 (by `retry-after`),
    /// 5xx and network failures. 400/401/403 are config errors, a retry does not
    /// fix them, we fail at once with the text from the API.
    async fn post_with_retries(&self, body: &Value) -> Result<Value> {
        let mut attempt = 0;

        loop {
            attempt += 1;

            let api = self.backend.name();
            let result = self
                .backend
                .request(&self.http)
                .header("content-type", "application/json")
                .json(body)
                .send()
                .await;

            let response = match result {
                Ok(r) => r,
                Err(e) if attempt < MAX_ATTEMPTS => {
                    let delay = backoff(attempt);
                    tracing::warn!(error = %e, attempt, ?delay, "network error, retrying");
                    tokio::time::sleep(delay).await;
                    continue;
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("request to the {api} API failed"))
                }
            };

            let status = response.status();

            if status.is_success() {
                return response
                    .json()
                    .await
                    .with_context(|| format!("the {api} API answer is not JSON"));
            }

            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs);

            let text = response.text().await.unwrap_or_default();

            let retryable = status.as_u16() == 429 || status.is_server_error();
            if retryable && attempt < MAX_ATTEMPTS {
                // An overloaded server (5xx) needs more time than a network glitch.
                let delay = retry_after.unwrap_or_else(|| {
                    if status.is_server_error() {
                        overload_backoff(attempt)
                    } else {
                        backoff(attempt)
                    }
                });
                let short: String = text.chars().take(200).collect();
                tracing::warn!(%status, attempt, ?delay, answer = %short, "the API returned a temporary error, retrying");
                tokio::time::sleep(delay).await;
                continue;
            }

            if is_limit(status, &text) {
                let details: String = text.chars().take(300).collect();
                return Err(LimitReached { api, details }.into());
            }
            bail!("the {api} API returned {status}: {text}");
        }
    }
}

/// What to do with the summaries before the final reduce.
#[derive(Debug)]
enum ReduceStep<'a> {
    /// They fit in one request — reduce.
    Ready,
    /// One more intermediate round with these groups.
    Compress(Vec<&'a [Digest]>),
    /// Nothing more to split or the rounds are used up: `ensure_fits` decides.
    Stuck,
}

/// A pure decision for one reduce step — without the network, so it can be tested.
fn plan_reduce(partials: &[Digest], stage: usize, limits: Limits) -> ReduceStep<'_> {
    if digest::partials_size(partials) <= limits.input {
        return ReduceStep::Ready;
    }
    if stage > limits.stages || partials.len() < 2 {
        return ReduceStep::Stuck;
    }

    let groups = digest::split_partials(partials, limits.batch);
    // As many groups as summaries — the round condenses nothing, it only
    // spends one request per summary.
    if groups.len() >= partials.len() {
        return ReduceStep::Stuck;
    }
    ReduceStep::Compress(groups)
}

/// The final request must fit in the window.
///
/// If not, it is more honest to fail than to send a request that surely does not fit or
/// to drop topics: the cursors have not moved, the posts stay for the next run,
/// and the owner sees what exactly hit the limit.
fn ensure_fits(prompt: &str, partials: usize, limits: Limits) -> Result<()> {
    let chars = prompt.chars().count();
    if chars <= limits.input {
        return Ok(());
    }
    bail!(
        "summaries of {partials} parts do not fit in one request: {chars} characters with a limit of {}. \
         Cursors not moved, the posts go into the next run; \
         lower lookback_hours or the number of channels",
        limits.input
    )
}

/// How many different channels are in a part. Posts come in a row by channel, so
/// it is enough to count changes of `channel_id`.
fn distinct_channels(posts: &[Post]) -> usize {
    let mut count = 0;
    let mut current = None;
    for post in posts {
        if current != Some(post.channel_id) {
            current = Some(post.channel_id);
            count += 1;
        }
    }
    count
}

/// Total token usage over all requests of the run. `None` only if no
/// answer sent the counters: otherwise we would show a dash where there was usage.
fn add(total: Option<u64>, next: Option<u64>) -> Option<u64> {
    match (total, next) {
        (None, next) => next,
        (total, None) => total,
        (Some(total), Some(next)) => Some(total + next),
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(2u64.pow(attempt))
}

/// 15 s, 30 s, 60 s: the server said it is overloaded.
fn overload_backoff(attempt: u32) -> Duration {
    Duration::from_secs(15 * 2u64.pow(attempt - 1))
}

/// We check `stop_reason` BEFORE reading `content`: on a refusal the model answers
/// HTTP 200 with `stop_reason: "refusal"` and empty content.
fn parse_summary(response: Value) -> Result<Summary> {
    let stop_reason = response
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or("");

    if stop_reason == "refusal" {
        let details = response.get("stop_details").cloned().unwrap_or(Value::Null);
        bail!("the model refused the request (stop_reason=refusal): {details}");
    }
    if stop_reason == "max_tokens" {
        bail!("the answer was cut at max_tokens — raise anthropic.max_tokens in the config");
    }

    let usage = |field: &str| {
        response
            .get("usage")
            .and_then(|u| u.get(field))
            .and_then(Value::as_u64)
    };

    let text: String = response
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .concat()
        })
        .unwrap_or_default();

    if text.trim().is_empty() {
        bail!("the Anthropic API answer has no text blocks: {response}");
    }

    let digest: Digest = serde_json::from_str(&text)
        .with_context(|| format!("could not parse the model answer as a digest: {text}"))?;

    tracing::info!(
        input_tokens = usage("input_tokens"),
        output_tokens = usage("output_tokens"),
        %stop_reason,
        topics = digest.topics.len(),
        "model answer received"
    );

    Ok(Summary {
        digest,
        input_tokens: usage("input_tokens"),
        output_tokens: usage("output_tokens"),
    })
}

/// The Gemini answer: text is in `candidates[0].content.parts`, parts with
/// `thought: true` are the model's thinking and are skipped. A blocked prompt
/// comes back with no candidates and `promptFeedback.blockReason`.
fn parse_gemini(response: Value) -> Result<Summary> {
    if let Some(reason) = response
        .pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str)
    {
        bail!("Gemini blocked the request (blockReason={reason})");
    }

    let candidate = response
        .pointer("/candidates/0")
        .with_context(|| format!("the Gemini answer has no candidates: {response}"))?;
    let finish_reason = candidate
        .get("finishReason")
        .and_then(Value::as_str)
        .unwrap_or("");
    if finish_reason == "MAX_TOKENS" {
        bail!("the answer was cut at max_tokens — raise gemini.max_tokens in the config");
    }
    if finish_reason != "STOP" && !finish_reason.is_empty() {
        bail!("Gemini stopped the answer (finishReason={finish_reason})");
    }

    let text: String = candidate
        .pointer("/content/parts")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p.get("thought").and_then(Value::as_bool) != Some(true))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .concat()
        })
        .unwrap_or_default();
    if text.trim().is_empty() {
        bail!("the Gemini answer has no text: {response}");
    }

    let digest: Digest = serde_json::from_str(&text)
        .with_context(|| format!("could not parse the model answer as a digest: {text}"))?;

    let usage = |field: &str| {
        response
            .pointer(&format!("/usageMetadata/{field}"))
            .and_then(Value::as_u64)
    };
    // Thinking tokens are paid as output, so we count them there.
    let output_tokens = add(usage("candidatesTokenCount"), usage("thoughtsTokenCount"));

    tracing::info!(
        input_tokens = usage("promptTokenCount"),
        output_tokens,
        %finish_reason,
        topics = digest.topics.len(),
        "model answer received"
    );

    Ok(Summary {
        digest,
        input_tokens: usage("promptTokenCount"),
        output_tokens,
    })
}
