//! Модель дайджеста: подстановка в промпт, JSON-схема ответа, проверка ссылок,
//! рендер в Telegram-HTML и разбиение по лимиту сообщения.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;

/// Лимит Telegram — 4096 символов на сообщение, и считает он их в UTF-16
/// (emoji = 2 единицы). Держим запас на служебную разметку.
const TELEGRAM_CHUNK_LIMIT: usize = 3900;

/// Один пост канала, уже отфильтрованный и со склеенным альбомом.
#[derive(Debug, Clone)]
pub struct Post {
    /// Id канала в базе — по нему считается курсор.
    pub channel_id: i64,
    /// Как канал адресуется: `@username` или `id 123`.
    pub channel: String,
    /// Отображаемое имя канала.
    pub title: String,
    pub date: DateTime<Utc>,
    pub text: String,
    pub url: String,
    /// Откуда репост, если это репост: без пометки модель считает репост и
    /// оригинал двумя разными новостями и не сливает их в один пункт.
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
    /// Публиковать нечего: заголовок без единой темы — это спам в канал.
    pub fn is_empty(&self) -> bool {
        self.topics.is_empty()
    }
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

/// Строки и таймзона для рендера. Живут в конфиге: вечерний прогон по
/// `America/New_York` иначе получал бы завтрашнюю дату из UTC.
pub struct RenderOptions<'a> {
    pub digest_title: &'a str,
    pub tldr_title: &'a str,
    pub timezone: Tz,
    pub now: DateTime<Utc>,
}

/// Схема структурированного ответа. Живёт в коде, а не в конфиге: это контракт
/// парсинга, менять его без правки `Digest` нельзя. Текстовые инструкции —
/// в `[prompts]`.
///
/// Ограничения API: `additionalProperties: false` обязателен, keywords вроде
/// `minLength`/`maximum` не поддерживаются.
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

/// Выкидываем из ответа всё, чего не было во входных постах.
///
/// Промпт просит «не придумывай URL», но просьба — не проверка. Заодно это
/// защита от prompt-injection: пост может уговорить модель вставить чужую
/// ссылку, но в дайджест попадут только адреса собранных постов.
/// Возвращает число выброшенных ссылок.
pub fn drop_foreign_sources(digest: &mut Digest, posts: &[Post]) -> usize {
    let allowed: HashSet<&str> = posts.iter().map(|p| p.url.as_str()).collect();
    let mut dropped = 0;

    for topic in &mut digest.topics {
        topic.sources.retain(|source| {
            let ok = allowed.contains(source.url.as_str());
            if !ok {
                dropped += 1;
                tracing::warn!(url = %source.url, topic = %topic.title, "ссылки не было во входных постах, выбрасываем");
            }
            ok
        });
    }
    dropped
}

/// Входные данные для модели: посты, сгруппированные по каналам, каждый со
/// ссылкой — модель обязана вернуть в `sources` именно эти URL.
pub fn format_posts(posts: &[Post]) -> String {
    let mut out = String::new();
    let mut current = i64::MIN;

    for post in posts {
        if post.channel_id != current {
            current = post.channel_id;
            out.push_str(&format!(
                "\n\n=== Канал: {} ({}) ===\n",
                post.title, post.channel
            ));
        }
        out.push_str(&format!(
            "\n--- {} | {}{}\n{}\n",
            post.date.format("%Y-%m-%d %H:%M UTC"),
            post.url,
            post.origin
                .as_ref()
                .map(|origin| format!(" | репост из: {origin}"))
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

/// То же самое, но с готовым блоком материалов: на сведении (reduce) вместо
/// постов подставляются сводки по частям.
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

/// Накладные расходы на один пост в промпте: дата, ссылка, разделители.
const POST_OVERHEAD_CHARS: usize = 200;

/// Разбить посты на части под map-reduce — по каналам.
///
/// Канал целиком уходит в одну часть: сжимать половину ленты канала в отрыве
/// от второй половины — значит терять контекст ровно там, где модель должна
/// слить дубли. Разрывается канал только если он сам не влезает в бюджет;
/// пост не разрывается никогда — молча терять хвост запрещено.
///
/// Посты приходят подряд по каналам, поэтому все части остаются непрерывными
/// срезами исходного списка.
pub fn split_batches(posts: &[Post], budget_chars: usize) -> Vec<&[Post]> {
    let mut batches: Vec<&[Post]> = Vec::new();
    let mut batch_start = 0;
    let mut batch_size = 0;

    for run in channel_runs(posts) {
        let posts_of_run = &posts[run.clone()];
        let run_size = batch_cost(posts_of_run);

        // Канал не влезает даже в пустую часть: режем его, но сначала
        // закрываем накопленное, чтобы не смешивать обрывок с соседями.
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

/// Границы каналов: посты идут подряд по каналам, так что достаточно отследить
/// смену `channel_id`.
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

/// Разбить сводки частей для промежуточного сведения.
///
/// Нужно, когда сводок столько, что их сумма сама не влезает в один запрос:
/// тогда они сводятся в несколько заходов. Сводка, которая одна больше
/// бюджета, уходит в группу одна — резать её нечем.
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

/// Во сколько символов обойдутся сводки в запросе.
pub fn partials_size(parts: &[Digest]) -> usize {
    format_partials(parts).chars().count()
}

/// Сводки частей — вход для сведения. Ссылки переносим как есть: итоговый
/// дайджест всё равно проверяется по множеству собранных постов, а без них
/// модели нечего будет указать в источниках.
pub fn format_partials(parts: &[Digest]) -> String {
    let mut out = String::new();

    for (index, part) in parts.iter().enumerate() {
        out.push_str(&format!("\n\n=== Сводка части {} ===\n", index + 1));
        for topic in &part.topics {
            out.push_str(&format!("\n--- {}\n{}\n", topic.title, topic.summary));
            if !topic.sources.is_empty() {
                let links: Vec<String> = topic
                    .sources
                    .iter()
                    .map(|s| format!("{} {}", s.channel, s.url))
                    .collect();
                out.push_str(&format!("Источники: {}\n", links.join(", ")));
            }
        }
        for item in &part.tldr {
            out.push_str(&format!("\n• {item}"));
        }
    }
    out
}

/// Рендер в Telegram-HTML. Разметку строим сами, а не просим у модели: чинить
/// сломанный markdown от LLM дороже, чем собрать HTML из структуры.
pub fn render_html(digest: &Digest, opts: &RenderOptions) -> Vec<String> {
    if digest.is_empty() {
        return Vec::new();
    }

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
        blocks.push(block);
    }

    pack(blocks)
}

/// Складываем блоки в сообщения по лимиту, не разрывая тему пополам.
/// Блок, который сам длиннее лимита, режем с сохранением HTML-разметки.
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

/// Разбиение обычного текста — ответов на команды и уведомлений — по тому же
/// лимиту сообщения.
///
/// Нужно потому, что Telegram длинное сообщение не обрезает, а отвергает:
/// `/list` на полусотне каналов или `/status` с длинной ошибкой иначе просто
/// не приходили бы. Режем по строкам, а строку длиннее лимита — по границам
/// символов; разметки здесь нет, ломать нечего.
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

/// Telegram считает длину сообщения в UTF-16: `chars().count()` на дайджесте
/// с эмодзи занижает её вдвое.
fn len16(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Telegram-HTML требует экранирования `&`, `<`, `>`; кавычку экранируем тоже,
/// иначе URL с `"` разорвал бы атрибут `href`.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(url: &str) -> Post {
        Post {
            channel_id: 1,
            channel: "@news".into(),
            title: "News".into(),
            date: Utc::now(),
            text: "текст".into(),
            url: url.into(),
            origin: None,
        }
    }

    fn topic(title: &str, urls: &[&str]) -> Topic {
        Topic {
            title: title.into(),
            summary: "итог".into(),
            sources: urls
                .iter()
                .map(|u| Source {
                    channel: "@news".into(),
                    url: (*u).into(),
                })
                .collect(),
        }
    }

    fn options() -> RenderOptions<'static> {
        RenderOptions {
            digest_title: "📰 Дайджест за {{date}}",
            tldr_title: "Коротко",
            timezone: chrono_tz::UTC,
            now: DateTime::parse_from_rfc3339("2026-09-15T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        }
    }

    #[test]
    fn escapes_html_specials_including_quotes() {
        assert_eq!(escape("a & b < c > d"), "a &amp; b &lt; c &gt; d");
        assert_eq!(escape("\"x\""), "&quot;x&quot;");
    }

    #[test]
    fn packs_blocks_without_exceeding_limit() {
        let blocks = vec!["x".repeat(3000), "y".repeat(3000), "z".repeat(100)];
        let messages = pack(blocks);
        assert_eq!(messages.len(), 2);
        assert!(messages.iter().all(|m| len16(m) <= TELEGRAM_CHUNK_LIMIT));
    }

    #[test]
    fn splits_a_single_oversized_block() {
        let block = (0..500)
            .map(|i| format!("строка {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let pieces = split_oversized(block);
        assert!(pieces.len() > 1);
        assert!(pieces.iter().all(|p| len16(p) <= TELEGRAM_CHUNK_LIMIT));
    }

    #[test]
    fn emoji_count_as_two_units_like_telegram_counts_them() {
        let block = "🎉".repeat(2500);
        assert_eq!(len16(&block), 5000);
        assert!(!split_oversized(block.clone()).is_empty());
        assert!(pack(vec![block])
            .iter()
            .all(|m| len16(m) <= TELEGRAM_CHUNK_LIMIT));
    }

    #[test]
    fn short_plain_text_is_sent_as_one_message() {
        assert_eq!(split_plain("привет"), vec!["привет".to_string()]);
        assert_eq!(split_plain(""), vec![String::new()]);
    }

    #[test]
    fn long_plain_text_is_split_on_line_boundaries() {
        // Ответ на /list в большом аккаунте: много коротких строк.
        let text = (0..400)
            .map(|i| format!("✅ канал {i} — id {i}, @name{i}, курсор {i}"))
            .collect::<Vec<_>>()
            .join("\n");

        let pieces = split_plain(&text);
        assert!(pieces.len() > 1);
        assert!(pieces.iter().all(|p| len16(p) <= TELEGRAM_CHUNK_LIMIT));
        // Ни одна строка не должна потеряться или разъехаться по двум частям.
        assert_eq!(pieces.join("\n"), text);
    }

    #[test]
    fn a_single_overlong_line_is_cut_by_character_boundaries() {
        // Одна строка без переводов: длинная ошибка в /status или уведомлении.
        let text = "я".repeat(5000);
        let pieces = split_plain(&text);
        assert!(pieces.len() > 1);
        assert!(pieces.iter().all(|p| len16(p) <= TELEGRAM_CHUNK_LIMIT));
        assert_eq!(pieces.concat(), text);

        // Суррогатная пара не должна разрезаться пополам.
        let text = "🎉".repeat(4000);
        let pieces = split_plain(&text);
        assert!(pieces.iter().all(|p| len16(p) <= TELEGRAM_CHUNK_LIMIT));
        assert_eq!(pieces.concat(), text);
    }

    #[test]
    fn oversized_html_keeps_text_entities_and_links_in_every_piece() {
        use grammers_client::parsers::parse_html_message;
        let text = "🎉 & <новость>\n".repeat(900);
        for html in [
            format!("<b>{}</b>", escape(&text)),
            format!("<a href=\"https://t.me/news/1\">{}</a>", escape(&text)),
        ] {
            let pieces = split_oversized(html);
            assert!(pieces.len() > 1);
            assert!(pieces.iter().all(|p| len16(p) <= TELEGRAM_CHUNK_LIMIT));
            let restored: String = pieces
                .iter()
                .map(|p| {
                    let (text, entities) = parse_html_message(p);
                    assert_eq!(entities.len(), 1);
                    text
                })
                .collect();
            assert_eq!(restored, text);
        }
    }

    fn sized_post(channel_id: i64, chars: usize, url: &str) -> Post {
        Post {
            channel_id,
            channel: format!("@c{channel_id}"),
            title: format!("Канал {channel_id}"),
            date: Utc::now(),
            text: "я".repeat(chars),
            url: url.into(),
            origin: None,
        }
    }

    #[test]
    fn batches_stay_within_the_budget_and_keep_every_post() {
        let posts: Vec<Post> = (0..10)
            .map(|i| sized_post(i / 5, 800, &format!("https://t.me/c/1/{i}")))
            .collect();

        let batches = split_batches(&posts, 3000);
        assert!(batches.len() > 1);

        // Ни один пост не потерян и порядок сохранён — иначе это тихая обрезка.
        let seen: Vec<&str> = batches
            .iter()
            .flat_map(|b| b.iter().map(|p| p.url.as_str()))
            .collect();
        let expected: Vec<&str> = posts.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(seen, expected);

        for batch in &batches {
            let size: usize = batch
                .iter()
                .map(|p| p.text.chars().count() + POST_OVERHEAD_CHARS)
                .sum();
            assert!(size <= 3000 || batch.len() == 1, "часть длиннее бюджета");
        }
    }

    #[test]
    fn a_channel_is_not_split_between_batches_unless_it_has_to_be() {
        // Три канала по ~2400 символов при бюджете 5000: первые два вместе,
        // третий отдельно — но ни один не разорван.
        let posts: Vec<Post> = (0..9)
            .map(|i| sized_post(i / 3, 600, &format!("https://t.me/c/1/{i}")))
            .collect();

        let batches = split_batches(&posts, 5000);
        assert_eq!(batches.len(), 2);
        for batch in &batches {
            let channels: Vec<i64> = batch
                .iter()
                .map(|p| p.channel_id)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            for channel in channels {
                let total = posts.iter().filter(|p| p.channel_id == channel).count();
                let here = batch.iter().filter(|p| p.channel_id == channel).count();
                assert_eq!(here, total, "канал {channel} разорван между частями");
            }
        }
    }

    #[test]
    fn a_channel_bigger_than_the_budget_is_split_but_keeps_its_neighbours_out() {
        let mut posts = vec![sized_post(1, 100, "https://t.me/c/1/1")];
        posts.extend((0..6).map(|i| sized_post(2, 900, &format!("https://t.me/c/2/{i}"))));
        posts.push(sized_post(3, 100, "https://t.me/c/3/1"));

        let batches = split_batches(&posts, 2500);
        // Мелкие соседи не приклеены к обрывкам большого канала.
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[0][0].channel_id, 1);
        assert_eq!(batches.last().unwrap()[0].channel_id, 3);

        let seen: Vec<&str> = batches
            .iter()
            .flat_map(|b| b.iter().map(|p| p.url.as_str()))
            .collect();
        assert_eq!(seen.len(), posts.len(), "посты потерялись при разбиении");
    }

    #[test]
    fn partials_are_grouped_for_a_staged_reduce() {
        let parts: Vec<Digest> = (0..6)
            .map(|i| Digest {
                topics: vec![topic(
                    &format!("тема {i}"),
                    &["https://t.me/news/1", "https://t.me/news/2"],
                )],
                tldr: vec![],
            })
            .collect();

        let one = partials_size(std::slice::from_ref(&parts[0]));
        let groups = split_partials(&parts, one * 2);

        assert_eq!(groups.len(), 3);
        assert_eq!(groups.iter().map(|g| g.len()).sum::<usize>(), parts.len());
        assert!(groups
            .iter()
            .all(|g| partials_size(g) <= one * 2 || g.len() == 1));
    }

    #[test]
    fn a_single_huge_partial_stays_alone_instead_of_looping_forever() {
        let parts = vec![
            Digest {
                topics: vec![Topic {
                    title: "огромная тема".into(),
                    summary: "я".repeat(10_000),
                    sources: vec![],
                }],
                tldr: vec![],
            },
            Digest {
                topics: vec![topic("маленькая", &["https://t.me/news/1"])],
                tldr: vec![],
            },
        ];

        let groups = split_partials(&parts, 1000);
        // Группы ровно по одной сводке: сжимать дальше нечем, и вызывающая
        // сторона на этом обязана остановиться, а не крутить цикл.
        assert_eq!(groups.len(), parts.len());
    }

    #[test]
    fn a_post_larger_than_the_budget_goes_into_its_own_batch() {
        let posts = vec![
            sized_post(1, 10, "https://t.me/c/1/1"),
            sized_post(1, 50_000, "https://t.me/c/1/2"),
            sized_post(1, 10, "https://t.me/c/1/3"),
        ];
        let batches = split_batches(&posts, 1000);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[1][0].url, "https://t.me/c/1/2");
    }

    #[test]
    fn everything_fits_into_one_batch_when_it_is_small() {
        let posts = vec![sized_post(1, 10, "https://t.me/c/1/1")];
        assert_eq!(split_batches(&posts, 100_000).len(), 1);
        assert!(split_batches(&[], 100_000).is_empty());
    }

    #[test]
    fn partial_summaries_carry_their_links_into_the_reduce_step() {
        // Ссылки — единственное, что связывает сводку с исходными постами:
        // без них свод не сможет заполнить sources, и все источники вылетят
        // на проверке.
        let parts = vec![
            Digest {
                topics: vec![topic("тема A", &["https://t.me/news/1"])],
                tldr: vec![],
            },
            Digest {
                topics: vec![topic("тема B", &["https://t.me/news/2"])],
                tldr: vec!["коротко".into()],
            },
        ];

        let text = format_partials(&parts);
        assert!(text.contains("https://t.me/news/1"));
        assert!(text.contains("https://t.me/news/2"));
        assert!(text.contains("тема A") && text.contains("тема B"));
        assert!(text.contains("Сводка части 1") && text.contains("Сводка части 2"));
    }

    #[test]
    fn prompt_placeholders_are_filled_for_both_steps() {
        let posts = vec![sized_post(1, 10, "https://t.me/c/1/1")];
        let template =
            "{{post_count}} постов из {{channel_count}} каналов за {{lookback_hours}} ч\n{{posts}}";

        let mapped = render_user_prompt(template, &posts, 24, 3, options().now, chrono_tz::UTC);
        assert!(mapped.starts_with("1 постов из 3 каналов за 24 ч"));
        assert!(mapped.contains("https://t.me/c/1/1"));

        let reduced =
            render_prompt_with(template, "сводка", 42, 24, 3, options().now, chrono_tz::UTC);
        assert!(reduced.starts_with("42 постов из 3 каналов за 24 ч"));
        assert!(reduced.contains("сводка"));
    }

    #[test]
    fn a_repost_is_marked_so_duplicates_can_be_merged() {
        let mut post = sized_post(1, 10, "https://t.me/c/1/1");
        post.origin = Some("Первоисточник (https://t.me/source/42)".into());

        let text = format_posts(&[post]);
        assert!(text.contains("репост из: Первоисточник (https://t.me/source/42)"));
    }

    #[test]
    fn foreign_urls_are_dropped() {
        let posts = vec![post("https://t.me/news/1")];
        let mut digest = Digest {
            topics: vec![topic(
                "тема",
                &["https://t.me/news/1", "https://evil.example/phish"],
            )],
            tldr: vec![],
        };

        assert_eq!(drop_foreign_sources(&mut digest, &posts), 1);
        assert_eq!(digest.topics[0].sources.len(), 1);
        assert_eq!(digest.topics[0].sources[0].url, "https://t.me/news/1");
    }

    #[test]
    fn empty_digest_renders_to_nothing() {
        let digest = Digest::default();
        assert!(render_html(&digest, &options()).is_empty());
    }

    #[test]
    fn renders_header_tldr_and_topic_without_sources() {
        let digest = Digest {
            topics: vec![topic("<заголовок>", &[])],
            tldr: vec!["первое".into()],
        };
        let html = render_html(&digest, &options()).join("\n");
        assert!(html.contains("📰 Дайджест за 15.09.2026"));
        assert!(html.contains("<b>Коротко</b>"));
        assert!(html.contains("&lt;заголовок&gt;"));
        assert!(!html.contains("<a href"));
    }

    #[test]
    fn header_date_uses_the_configured_timezone() {
        let mut opts = options();
        opts.timezone = chrono_tz::America::New_York;
        opts.now = DateTime::parse_from_rfc3339("2026-09-16T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let digest = Digest {
            topics: vec![topic("тема", &[])],
            tldr: vec![],
        };
        let html = render_html(&digest, &opts).join("\n");
        assert!(
            html.contains("15.09.2026"),
            "вечер 15-го в Нью-Йорке: {html}"
        );
    }
}
