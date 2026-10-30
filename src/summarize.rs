//! Calls to the Anthropic Messages API.
//!
//! There is no official Rust SDK, so we call HTTP directly with reqwest —
//! this is the normal way for languages without an SDK.

use anyhow::{bail, Context, Result};
use chrono::Utc;
use chrono_tz::Tz;
use serde_json::{json, Value};
use std::time::Duration;

use crate::cancel::Cancel;
use crate::config::{Anthropic, Prompts};
use crate::digest::{self, Digest, Post};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_ATTEMPTS: u32 = 4;

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
    cfg: AnthropicSettings,
    limits: Limits,
}

/// A copy of the settings without a lifetime tied to the whole Config.
struct AnthropicSettings {
    api_key: String,
    api_url: String,
    model: String,
    effort: String,
    max_tokens: u32,
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
    pub fn new(cfg: &Anthropic) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.request_timeout_secs))
            .build()
            .context("could not create the HTTP client")?;

        Ok(Self {
            http,
            cfg: AnthropicSettings {
                api_key: cfg.api_key.clone(),
                api_url: cfg.api_url.clone(),
                model: cfg.model.clone(),
                effort: cfg.effort.clone(),
                max_tokens: cfg.max_tokens,
            },
            limits: Limits::default(),
        })
    }

    #[cfg(test)]
    fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
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
        // thinking is on by default on claude-opus-5 — we do not pass the parameter.
        // temperature and budget_tokens return 400 on this model.
        let body = json!({
            "model": self.cfg.model,
            "max_tokens": self.cfg.max_tokens,
            "output_config": {
                "effort": self.cfg.effort,
                "format": { "type": "json_schema", "schema": digest::response_schema() }
            },
            "system": system,
            "messages": [{ "role": "user", "content": user }]
        });

        let response = tokio::select! {
            _ = cancel.cancelled() => bail!("model request stopped by the shutdown signal"),
            result = self.post_with_retries(&body) => result?,
        };

        parse_summary(response)
    }

    /// We retry only what makes sense to retry: 429 (by `retry-after`),
    /// 5xx and network failures. 400/401/403 are config errors, a retry does not
    /// fix them, we fail at once with the text from the API.
    async fn post_with_retries(&self, body: &Value) -> Result<Value> {
        let mut attempt = 0;

        loop {
            attempt += 1;

            let result = self
                .http
                .post(&self.cfg.api_url)
                .header("x-api-key", &self.cfg.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
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
                Err(e) => return Err(e).context("request to the Anthropic API failed"),
            };

            let status = response.status();

            if status.is_success() {
                return response
                    .json()
                    .await
                    .context("the Anthropic API answer is not JSON");
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
                let delay = retry_after.unwrap_or_else(|| backoff(attempt));
                tracing::warn!(%status, attempt, ?delay, "the API returned a temporary error, retrying");
                tokio::time::sleep(delay).await;
                continue;
            }

            bail!("the Anthropic API returned {status}: {text}");
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn response(body: Value) -> Value {
        body
    }

    fn post(channel_id: i64) -> Post {
        Post {
            channel_id,
            channel: format!("@c{channel_id}"),
            title: format!("Channel {channel_id}"),
            date: Utc::now(),
            text: "text".into(),
            url: format!("https://t.me/c{channel_id}/1"),
            origin: None,
        }
    }

    /// An Anthropic answer with a ready digest inside a text block.
    fn api_reply(summary_chars: usize, topics: usize) -> Value {
        let digest = json!({
            "topics": (0..topics).map(|i| json!({
                "title": format!("topic {i}"),
                "summary": "x".repeat(summary_chars),
                "sources": [{ "channel": "@c1", "url": "https://t.me/c1/1" }]
            })).collect::<Vec<_>>(),
            "tldr": []
        });
        json!({
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 10, "output_tokens": 3 },
            "content": [{ "type": "text", "text": digest.to_string() }]
        })
    }

    /// A fake Anthropic on localhost: counts requests and answers with ready
    /// answers. It is needed to test the map-reduce flow itself —
    /// how many requests went out and whether the final one went out at all.
    struct FakeApi {
        url: String,
        requests: Arc<AtomicUsize>,
    }

    async fn fake_api(bodies: Vec<Value>) -> FakeApi {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);

        tokio::spawn(async move {
            let bodies: Vec<String> = bodies.iter().map(|b| b.to_string()).collect();
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let index = counter.fetch_add(1, Ordering::SeqCst);

                // We read the whole request: otherwise the client gets a broken
                // connection while writing the body instead of an answer.
                let mut data = Vec::new();
                let mut chunk = vec![0u8; 16 * 1024];
                while let Ok(read) = socket.read(&mut chunk).await {
                    if read == 0 {
                        break;
                    }
                    data.extend_from_slice(&chunk[..read]);
                    if let Some(head) = find_headers_end(&data) {
                        if data.len() >= head + content_length(&data[..head]) {
                            break;
                        }
                    }
                }

                let body = bodies
                    .get(index)
                    .or_else(|| bodies.last())
                    .cloned()
                    .unwrap_or_default();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        FakeApi { url, requests }
    }

    fn find_headers_end(data: &[u8]) -> Option<usize> {
        data.windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| p + 4)
    }

    fn content_length(head: &[u8]) -> usize {
        String::from_utf8_lossy(head)
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())?
            })
            .unwrap_or(0)
    }

    fn summarizer(url: &str) -> Summarizer {
        Summarizer::new(&crate::config::Anthropic {
            api_key: "test".into(),
            model: "claude-opus-5".into(),
            effort: "low".into(),
            max_tokens: 16000,
            api_url: url.into(),
            request_timeout_secs: 30,
        })
        .unwrap()
    }

    fn prompts() -> Prompts {
        Prompts {
            system: "reduce".into(),
            user_template: "{{post_count}} posts\n{{posts}}".into(),
            map_system: None,
            digest_title: "Digest for {{date}}".into(),
            tldr_title: "In short".into(),
        }
    }

    /// Input that surely goes to map-reduce: `channels` channels
    /// with `per_channel` posts each, each post `chars` characters long.
    fn big_input(channels: i64, per_channel: usize, chars: usize) -> Vec<Post> {
        let mut posts = Vec::new();
        for channel in 1..=channels {
            for index in 0..per_channel {
                posts.push(Post {
                    channel_id: channel,
                    channel: format!("@c{channel}"),
                    title: format!("Channel {channel}"),
                    date: Utc::now(),
                    text: "x".repeat(chars),
                    url: format!("https://t.me/c{channel}/{index}"),
                    origin: None,
                });
            }
        }
        posts
    }

    #[test]
    fn channels_are_counted_per_batch_not_per_run() {
        assert_eq!(distinct_channels(&[]), 0);
        assert_eq!(distinct_channels(&[post(1), post(1), post(2)]), 2);
    }

    fn partial(summary_chars: usize) -> Digest {
        Digest {
            topics: vec![crate::digest::Topic {
                title: "topic".into(),
                summary: "x".repeat(summary_chars),
                sources: vec![],
            }],
            tldr: vec![],
        }
    }

    #[test]
    fn small_partials_go_straight_to_the_final_reduce() {
        let partials = vec![partial(100), partial(100)];
        assert!(matches!(
            plan_reduce(&partials, 1, Limits::default()),
            ReduceStep::Ready
        ));
    }

    #[test]
    fn oversized_partials_are_compressed_in_groups() {
        // Eight summaries of ~60k: 480k together, but three fit in a group.
        let partials: Vec<Digest> = (0..8).map(|_| partial(60_000)).collect();
        match plan_reduce(&partials, 1, Limits::default()) {
            ReduceStep::Compress(groups) => {
                assert!(groups.len() < partials.len());
                assert!(groups
                    .iter()
                    .all(|g| digest::partials_size(g) <= MAP_BATCH_CHARS));
            }
            other => panic!("expected an intermediate reduce, got {other:?}"),
        }
    }

    #[test]
    fn partials_that_cannot_be_paired_stop_the_loop() {
        // Four summaries of 120k: each fits in the budget, but even two together
        // do not. Grouping gives nothing, there is no point in spinning rounds.
        let partials: Vec<Digest> = (0..4).map(|_| partial(120_000)).collect();
        assert!(matches!(
            plan_reduce(&partials, 1, Limits::default()),
            ReduceStep::Stuck
        ));
    }

    #[test]
    fn the_stage_budget_is_finite() {
        let partials: Vec<Digest> = (0..8).map(|_| partial(60_000)).collect();
        assert!(matches!(
            plan_reduce(&partials, MAX_REDUCE_STAGES + 1, Limits::default()),
            ReduceStep::Stuck
        ));
    }

    #[test]
    fn an_oversized_final_prompt_is_an_error_not_a_request() {
        assert!(ensure_fits("short", 2, Limits::default()).is_ok());

        let error = ensure_fits(&"x".repeat(LARGE_INPUT_CHARS + 1), 4, Limits::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("do not fit in one request"), "{error}");
        assert!(error.contains("Cursors not moved"), "{error}");
    }

    #[tokio::test]
    async fn map_reduce_sends_one_request_per_part_plus_the_reduce() {
        // 3 channels of 180k characters: they do not fit in one request, but fit in three.
        let posts = big_input(3, 45, 3_800);
        let api = fake_api(vec![api_reply(100, 2)]).await;

        let summary = summarizer(&api.url)
            .summarize(&prompts(), &posts, 24, 3, chrono_tz::UTC, &Cancel::never())
            .await
            .unwrap();

        assert_eq!(
            api.requests.load(Ordering::SeqCst),
            4,
            "three condense calls and one reduce"
        );
        assert_eq!(summary.digest.topics.len(), 2);
        // Tokens are summed over all requests, not taken from the last one.
        assert_eq!(summary.input_tokens, Some(40));
        assert_eq!(summary.output_tokens, Some(12));
    }

    #[tokio::test]
    async fn an_unshrinkable_reduce_fails_before_spending_the_final_request() {
        // Each summary alone is 150k: pairs do not fit together, an intermediate
        // reduce does not help.
        let posts = big_input(3, 45, 3_800);
        let api = fake_api(vec![api_reply(150_000, 1)]).await;

        let error = summarizer(&api.url)
            .summarize(&prompts(), &posts, 24, 3, chrono_tz::UTC, &Cancel::never())
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("do not fit in one request"), "{error}");
        assert_eq!(
            api.requests.load(Ordering::SeqCst),
            3,
            "the final request is not sent, because it surely does not fit"
        );
    }

    /// Smaller limits: the same flow, but the input is measured in hundreds of
    /// characters, not hundreds of thousands. The ratio `input = 2 × batch` is the same as
    /// in production, or the test would check the wrong arithmetic.
    fn tiny_limits(stages: usize) -> Limits {
        Limits {
            input: 2_000,
            batch: 1_000,
            stages,
        }
    }

    /// The summary the mock returns for such an answer: we calculate expectations with the
    /// same code as production, not in our heads.
    fn partial_from(reply: &Value) -> Digest {
        parse_summary(reply.clone()).unwrap().digest
    }

    #[tokio::test]
    async fn partials_that_do_not_fit_are_compressed_before_the_final_reduce() {
        // Six channels with two posts each: each channel is its own part (budget 1000).
        let posts = big_input(6, 2, 240);
        let map_reply = api_reply(400, 1);
        let limits = tiny_limits(3);

        // Test preconditions: the summaries of six parts do not fit together, but
        // can be grouped — so an intermediate round must happen.
        let partials: Vec<Digest> = (0..6).map(|_| partial_from(&map_reply)).collect();
        assert!(digest::partials_size(&partials) > limits.input);
        let groups = digest::split_partials(&partials, limits.batch).len();
        assert!(groups < partials.len());

        // The first six answers are large summaries, then the mock returns a short one:
        // after the intermediate round everything fits in the final request.
        let mut bodies = vec![map_reply; 6];
        bodies.push(api_reply(50, 1));
        let api = fake_api(bodies).await;

        let summary = summarizer(&api.url)
            .with_limits(limits)
            .summarize(&prompts(), &posts, 24, 6, chrono_tz::UTC, &Cancel::never())
            .await
            .unwrap();

        let requests = api.requests.load(Ordering::SeqCst);
        assert_eq!(
            requests,
            6 + groups + 1,
            "six condense calls, intermediate groups and one reduce"
        );
        assert_eq!(summary.digest.topics.len(), 1);
        assert_eq!(
            summary.input_tokens,
            Some(10 * requests as u64),
            "tokens are counted over all requests, including intermediate ones"
        );
    }

    #[tokio::test]
    async fn stages_run_out_and_the_final_prompt_is_still_checked() {
        // Sixteen parts and exactly one allowed round: after it there are still
        // too many summaries. The rounds are used up — but the final check
        // must work and not let a bad request go out.
        let posts = big_input(16, 2, 240);
        let map_reply = api_reply(400, 1);
        let limits = tiny_limits(1);

        let partials: Vec<Digest> = (0..16).map(|_| partial_from(&map_reply)).collect();
        let groups = digest::split_partials(&partials, limits.batch).len();
        let after_stage: Vec<Digest> = (0..groups).map(|_| partial_from(&map_reply)).collect();
        assert!(
            digest::partials_size(&after_stage) > limits.input,
            "after the only round the summaries still do not fit"
        );

        let api = fake_api(vec![map_reply]).await;

        let error = summarizer(&api.url)
            .with_limits(limits)
            .summarize(&prompts(), &posts, 24, 16, chrono_tz::UTC, &Cancel::never())
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("do not fit in one request"), "{error}");
        assert_eq!(
            api.requests.load(Ordering::SeqCst),
            16 + groups,
            "sixteen condense calls and one round; the final request not sent"
        );
    }

    #[tokio::test]
    async fn a_small_input_stays_a_single_request() {
        let api = fake_api(vec![api_reply(100, 1)]).await;

        let summary = summarizer(&api.url)
            .summarize(
                &prompts(),
                &[post(1), post(2)],
                24,
                2,
                chrono_tz::UTC,
                &Cancel::never(),
            )
            .await
            .unwrap();

        assert_eq!(api.requests.load(Ordering::SeqCst), 1);
        assert_eq!(summary.digest.topics.len(), 1);
    }

    #[test]
    fn token_usage_adds_up_across_map_and_reduce() {
        assert_eq!(add(None, None), None);
        assert_eq!(add(None, Some(5)), Some(5));
        assert_eq!(add(Some(5), None), Some(5));
        assert_eq!(add(Some(5), Some(7)), Some(12));
    }

    #[test]
    fn refusal_is_reported_before_reading_content() {
        let error = parse_summary(response(json!({
            "stop_reason": "refusal",
            "stop_details": { "type": "refusal" },
            "content": []
        })))
        .unwrap_err()
        .to_string();
        assert!(error.contains("refusal"), "{error}");
    }

    #[test]
    fn truncated_answer_points_at_max_tokens() {
        let error = parse_summary(response(json!({
            "stop_reason": "max_tokens",
            "content": [{ "type": "text", "text": "{}" }]
        })))
        .unwrap_err()
        .to_string();
        assert!(error.contains("max_tokens"), "{error}");
    }

    #[test]
    fn empty_content_is_an_error_not_an_empty_digest() {
        assert!(parse_summary(response(json!({
            "stop_reason": "end_turn",
            "content": []
        })))
        .is_err());
    }

    #[test]
    fn several_text_blocks_are_concatenated() {
        let summary = parse_summary(response(json!({
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 10, "output_tokens": 20 },
            "content": [
                { "type": "thinking", "thinking": "…" },
                { "type": "text", "text": "{\"topics\":[],\"tl" },
                { "type": "text", "text": "dr\":[\"one\"]}" }
            ]
        })))
        .unwrap();

        assert_eq!(summary.digest.tldr, vec!["one".to_string()]);
        assert_eq!(summary.input_tokens, Some(10));
        assert_eq!(summary.output_tokens, Some(20));
    }

    #[test]
    fn invalid_json_from_the_model_is_reported_with_its_text() {
        let error = parse_summary(response(json!({
            "stop_reason": "end_turn",
            "content": [{ "type": "text", "text": "not json" }]
        })))
        .unwrap_err()
        .to_string();
        assert!(error.contains("not json"), "{error}");
    }
}
