//! The digest model: prompt filling, the JSON schema of the answer, link checks,
//! rendering to Telegram-HTML and splitting by the message limit.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;

/// The Telegram limit is 4096 characters per message, and it counts them in UTF-16
/// (emoji = 2 units). We keep a margin for service markup.
const TELEGRAM_CHUNK_LIMIT: usize = 3900;

/// One channel post, already filtered and with the album merged.
#[derive(Debug, Clone)]
pub struct Post {
    /// Channel id in the database — the cursor is counted by it.
    pub channel_id: i64,
    /// How the channel is named: `@username` or `id 123`.
    pub channel: String,
    /// The display name of the channel.
    pub title: String,
    pub date: DateTime<Utc>,
    pub text: String,
    pub url: String,
    /// Where the repost is from, if it is a repost: without the label the model sees a repost
    /// and the original as two different news items and does not merge them into one item.
    pub origin: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Digest {
    #[serde(default)]
    pub topics: Vec<Topic>,
    #[serde(default)]
    pub tldr: Vec<String>,
}

impl Digest {
    /// Nothing to post: a title without a single topic is spam in the channel.
    pub fn is_empty(&self) -> bool {
        self.topics.is_empty()
    }

    /// Why the answer looks broken, if it does. Sometimes the model writes the short
    /// summary in full, but then only a few topics and a stub like "placeholder". Such
    /// a digest must not be posted: the cursors would move, and the news would be lost.
    pub fn defect(&self) -> Option<String> {
        if self.topics.len() < self.tldr.len() {
            return Some(format!(
                "{} items in the short summary, but only {} topics",
                self.tldr.len(),
                self.topics.len()
            ));
        }
        for topic in &self.topics {
            let title = topic.title.trim();
            let summary = topic.summary.trim();
            if title.is_empty() || summary.is_empty() || is_stub(title) || is_stub(summary) {
                return Some(format!("a topic is a stub: title {title:?}"));
            }
        }
        None
    }
}

fn is_stub(text: &str) -> bool {
    let lower = text.to_lowercase();
    ["placeholder", "todo", "tbd", "lorem ipsum", "..."]
        .iter()
        .any(|stub| lower == *stub)
        || lower.contains("placeholder")
}

#[derive(Debug, Deserialize)]
pub struct Topic {
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub sources: Vec<Source>,
}

#[derive(Debug, Deserialize)]
pub struct Source {
    pub channel: String,
    pub url: String,
}

/// Strings and time zone for rendering. They live in the config: otherwise an evening run in
/// `America/New_York` would get tomorrow's date from UTC.
pub struct RenderOptions<'a> {
    pub digest_title: &'a str,
    pub tldr_title: &'a str,
    pub timezone: Tz,
    pub now: DateTime<Utc>,
}

/// The schema of the structured answer. It lives in the code, not in the config: it is the
/// parsing contract, you cannot change it without changing `Digest`. Text instructions are
/// in `[prompts]`.
///
/// API limits: `additionalProperties: false` is required, keywords like
/// `minLength`/`maximum` are not supported.
pub fn response_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["topics", "tldr"],
        "properties": {
            "topics": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["title", "summary", "sources"],
                    "properties": {
                        "title":   { "type": "string" },
                        "summary": { "type": "string" },
                        "sources": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["channel", "url"],
                                "properties": {
                                    "channel": { "type": "string" },
                                    "url":     { "type": "string" }
                                }
                            }
                        }
                    }
                }
            },
            "tldr": { "type": "array", "items": { "type": "string" } }
        }
    })
}

/// We drop from the answer everything that was not in the input posts.
///
/// The prompt says "do not make up URLs", but a request is not a check. This is also
/// protection from prompt injection: a post can talk the model into adding a foreign
/// link, but only addresses of collected posts get into the digest.
/// Returns the number of dropped links.
pub fn drop_foreign_sources(digest: &mut Digest, posts: &[Post]) -> usize {
    let allowed: HashSet<&str> = posts.iter().map(|p| p.url.as_str()).collect();
    let mut dropped = 0;

    for topic in &mut digest.topics {
        topic.sources.retain(|source| {
            let ok = allowed.contains(source.url.as_str());
            if !ok {
                dropped += 1;
                tracing::warn!(url = %source.url, topic = %topic.title, "the link was not in the input posts, dropping it");
            }
            ok
        });
    }
    dropped
}

/// Input for the model: posts grouped by channel, each with a link —
/// the model must return exactly these URLs in `sources`.
pub fn format_posts(posts: &[Post]) -> String {
    let mut out = String::new();
    let mut current = i64::MIN;

    for post in posts {
        if post.channel_id != current {
            current = post.channel_id;
            out.push_str(&format!(
                "\n\n=== Channel: {} ({}) ===\n",
                post.title, post.channel
            ));
        }
        out.push_str(&format!(
            "\n--- {} | {}{}\n{}\n",
            post.date.format("%Y-%m-%d %H:%M UTC"),
            post.url,
            post.origin
                .as_ref()
                .map(|origin| format!(" | repost from: {origin}"))
                .unwrap_or_default(),
            post.text.trim()
        ));
    }
    out
}

pub fn render_user_prompt(
    template: &str,
    posts: &[Post],
    lookback_hours: i64,
    channel_count: usize,
    now: DateTime<Utc>,
    timezone: Tz,
) -> String {
    render_prompt_with(
        template,
        &format_posts(posts),
        posts.len(),
        lookback_hours,
        channel_count,
        now,
        timezone,
    )
}

/// The same, but with a ready block of material: in the reduce step summaries of
/// parts are used instead of posts.
#[allow(clippy::too_many_arguments)]
pub fn render_prompt_with(
    template: &str,
    posts_block: &str,
    post_count: usize,
    lookback_hours: i64,
    channel_count: usize,
    now: DateTime<Utc>,
    timezone: Tz,
) -> String {
    template
        .replace(
            "{{now}}",
            &now.with_timezone(&timezone)
                .format("%Y-%m-%d %H:%M %Z")
                .to_string(),
        )
        .replace("{{lookback_hours}}", &lookback_hours.to_string())
        .replace("{{post_count}}", &post_count.to_string())
        .replace("{{channel_count}}", &channel_count.to_string())
        .replace("{{posts}}", posts_block)
}

/// Overhead of one post in the prompt: date, link, separators.
const POST_OVERHEAD_CHARS: usize = 200;

/// Split posts into parts for map-reduce — by channel.
///
/// A whole channel goes into one part: condensing half of a channel feed apart
/// from the other half means losing context exactly where the model must
/// merge duplicates. A channel is split only if it alone does not fit the budget;
/// a post is never split — silently losing the tail is forbidden.
///
/// Posts come in a row by channel, so all parts stay continuous
/// slices of the original list.
pub fn split_batches(posts: &[Post], budget_chars: usize) -> Vec<&[Post]> {
    let mut batches: Vec<&[Post]> = Vec::new();
    let mut batch_start = 0;
    let mut batch_size = 0;

    for run in channel_runs(posts) {
        let posts_of_run = &posts[run.clone()];
        let run_size = batch_cost(posts_of_run);

        // The channel does not fit even into an empty part: we cut it, but first
        // we close what has been collected, so we do not mix a piece with its neighbours.
        if run_size > budget_chars {
            if batch_size > 0 {
                batches.push(&posts[batch_start..run.start]);
                batch_size = 0;
            }
            let mut start = run.start;
            let mut size = 0;
            for index in run.clone() {
                let cost = post_cost(&posts[index]);
                if size > 0 && size + cost > budget_chars {
                    batches.push(&posts[start..index]);
                    start = index;
                    size = 0;
                }
                size += cost;
            }
            batches.push(&posts[start..run.end]);
            batch_start = run.end;
            continue;
        }

        if batch_size > 0 && batch_size + run_size > budget_chars {
            batches.push(&posts[batch_start..run.start]);
            batch_start = run.start;
            batch_size = 0;
        }
        batch_size += run_size;
    }

    if batch_start < posts.len() {
        batches.push(&posts[batch_start..]);
    }
    batches
}

/// Channel borders: posts come in a row by channel, so it is enough to watch
/// for a change of `channel_id`.
fn channel_runs(posts: &[Post]) -> Vec<std::ops::Range<usize>> {
    let mut runs: Vec<std::ops::Range<usize>> = Vec::new();

    for (index, post) in posts.iter().enumerate() {
        match runs.last_mut() {
            Some(run) if posts[run.start].channel_id == post.channel_id => run.end = index + 1,
            _ => runs.push(index..index + 1),
        }
    }
    runs
}

fn post_cost(post: &Post) -> usize {
    post.text.chars().count() + POST_OVERHEAD_CHARS
}

fn batch_cost(posts: &[Post]) -> usize {
    posts.iter().map(post_cost).sum()
}

/// Split part summaries for an intermediate reduce.
///
/// Needed when there are so many summaries that together they do not fit in one request:
/// then they are reduced in several rounds. A summary that alone is bigger than
/// the budget goes into a group alone — there is nothing to cut it with.
pub fn split_partials(parts: &[Digest], budget_chars: usize) -> Vec<&[Digest]> {
    let mut groups = Vec::new();
    let mut start = 0;
    let mut size = 0;

    for (index, part) in parts.iter().enumerate() {
        let cost = partials_size(std::slice::from_ref(part));
        if size > 0 && size + cost > budget_chars {
            groups.push(&parts[start..index]);
            start = index;
            size = 0;
        }
        size += cost;
    }
    if start < parts.len() {
        groups.push(&parts[start..]);
    }
    groups
}

/// How many characters the summaries will take in the request.
pub fn partials_size(parts: &[Digest]) -> usize {
    format_partials(parts).chars().count()
}

/// Part summaries are the input for the reduce. We copy the links as they are: the final
/// digest is checked against the set of collected posts anyway, and without them
/// the model would have nothing to put in the sources.
pub fn format_partials(parts: &[Digest]) -> String {
    let mut out = String::new();

    for (index, part) in parts.iter().enumerate() {
        out.push_str(&format!("\n\n=== Summary of part {} ===\n", index + 1));
        for topic in &part.topics {
            out.push_str(&format!("\n--- {}\n{}\n", topic.title, topic.summary));
            if !topic.sources.is_empty() {
                let links: Vec<String> = topic
                    .sources
                    .iter()
                    .map(|s| format!("{} {}", s.channel, s.url))
                    .collect();
                out.push_str(&format!("Sources: {}\n", links.join(", ")));
            }
        }
        for item in &part.tldr {
            out.push_str(&format!("\n• {item}"));
        }
    }
    out
}

/// Rendering to Telegram-HTML. We build the markup ourselves and do not ask the model: fixing
/// broken markdown from an LLM costs more than building HTML from the structure.
pub fn render_html(digest: &Digest, opts: &RenderOptions) -> Vec<String> {
    if digest.is_empty() {
        return Vec::new();
    }

    // The first message is the title with "In short", then every topic is its own
    // message. `pack` still cuts a block that alone is longer than the limit.
    let mut blocks: Vec<String> = Vec::new();

    let date = opts
        .now
        .with_timezone(&opts.timezone)
        .format("%d.%m.%Y")
        .to_string();
    blocks.push(format!(
        "<b>{}</b>",
        escape(&opts.digest_title.replace("{{date}}", &date))
    ));

    if !digest.tldr.is_empty() {
        let mut tldr = format!("<b>{}</b>", escape(opts.tldr_title));
        for item in &digest.tldr {
            tldr.push_str(&format!("\n• {}", escape(item)));
        }
        blocks.push(tldr);
    }
    let mut messages = pack(blocks);

    for topic in &digest.topics {
        let mut block = format!(
            "<b>{}</b>\n{}",
            escape(&topic.title),
            escape(&topic.summary)
        );
        if !topic.sources.is_empty() {
            let links: Vec<String> = topic
                .sources
                .iter()
                .map(|s| format!("<a href=\"{}\">{}</a>", escape(&s.url), escape(&s.channel)))
                .collect();
            block.push_str(&format!("\n{}", links.join(" · ")));
        }
        messages.extend(pack(vec![block]));
    }

    messages
}

/// We put blocks into messages by the limit, without cutting a topic in half.
/// A block that alone is longer than the limit is cut, keeping the HTML markup.
fn pack(blocks: Vec<String>) -> Vec<String> {
    let mut messages = Vec::new();
    let mut current = String::new();

    for block in blocks {
        for piece in split_oversized(block) {
            if current.is_empty() {
                current = piece;
            } else if len16(&current) + len16(&piece) + 2 <= TELEGRAM_CHUNK_LIMIT {
                current.push_str("\n\n");
                current.push_str(&piece);
            } else {
                messages.push(std::mem::take(&mut current));
                current = piece;
            }
        }
    }
    if !current.is_empty() {
        messages.push(current);
    }
    messages
}

fn split_oversized(block: String) -> Vec<String> {
    if len16(&block) <= TELEGRAM_CHUNK_LIMIT {
        return vec![block];
    }
    split_html(&block)
}

/// Split generated HTML without cutting an entity, emoji, or formatting tag.
/// Only <b> and <a> are emitted by the renderer; reopen them in each piece.
fn split_html(mut line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut tags: Vec<(&str, &str)> = Vec::new();
    let mut length = 0;
    while !line.is_empty() {
        let end = if line.starts_with('<') {
            line.find('>').unwrap() + 1
        } else if line.starts_with('&') {
            line.find(';').unwrap() + 1
        } else {
            line.chars().next().unwrap().len_utf8()
        };
        let (token, rest) = line.split_at(end);
        let closing = token.starts_with("</");
        let close_tag = if token == "<b>" {
            Some("</b>")
        } else if token.starts_with("<a ") {
            Some("</a>")
        } else {
            None
        };
        let reserve: usize = tags.iter().map(|(_, close)| len16(close)).sum();
        let extra = close_tag.map(len16).unwrap_or(0);
        if !closing && length + len16(token) + reserve + extra > TELEGRAM_CHUNK_LIMIT {
            for (_, close) in tags.iter().rev() {
                current.push_str(close);
            }
            out.push(std::mem::take(&mut current));
            for (open, _) in &tags {
                current.push_str(open);
            }
            length = len16(&current);
        }
        current.push_str(token);
        length += len16(token);
        if closing {
            tags.pop();
        } else if let Some(close) = close_tag {
            tags.push((token, close));
        }
        line = rest;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Splitting plain text — command replies and notifications — by the same
/// message limit.
///
/// Needed because Telegram does not cut a long message, it rejects it:
/// `/list` with fifty channels or `/status` with a long error would otherwise just
/// not arrive. We cut by lines, and a line longer than the limit — by character
/// borders; there is no markup here, nothing to break.
pub fn split_plain(text: &str) -> Vec<String> {
    if len16(text) <= TELEGRAM_CHUNK_LIMIT {
        return vec![text.to_string()];
    }

    let mut out = Vec::new();
    let mut current = String::new();

    for line in text.lines() {
        for piece in split_long_line(line) {
            if !current.is_empty() && len16(&current) + len16(&piece) + 1 > TELEGRAM_CHUNK_LIMIT {
                out.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(&piece);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn split_long_line(line: &str) -> Vec<String> {
    if len16(line) <= TELEGRAM_CHUNK_LIMIT {
        return vec![line.to_string()];
    }

    let mut out = Vec::new();
    let mut current = String::new();
    let mut length = 0;
    for symbol in line.chars() {
        let width = symbol.len_utf16();
        if length + width > TELEGRAM_CHUNK_LIMIT {
            out.push(std::mem::take(&mut current));
            length = 0;
        }
        current.push(symbol);
        length += width;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Telegram counts message length in UTF-16: `chars().count()` on a digest
/// with emoji gives half the real length.
fn len16(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Telegram-HTML needs `&`, `<`, `>` to be escaped; we escape the quote too,
/// or a URL with `"` would break the `href` attribute.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
