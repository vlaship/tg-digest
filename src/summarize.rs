//! Вызов Anthropic Messages API.
//!
//! Официального Rust SDK нет, поэтому дёргаем HTTP напрямую через reqwest —
//! это штатный путь для языков без SDK.

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

/// Выше этого объёма переходим на map-reduce: сжимаем вход частями, затем
/// сводим сводки в один дайджест. Контекст модели 1M токенов, так что порог
/// с запасом — но молча обрезать хвост нельзя ни при каком объёме.
const LARGE_INPUT_CHARS: usize = 400_000;

/// Бюджет одной части на шаге map. Половина порога: если вход только-только
/// перевалил за него, получаются две сопоставимые части, а не «почти всё
/// и кусочек».
const MAP_BATCH_CHARS: usize = LARGE_INPUT_CHARS / 2;

/// Сколько раз сводки можно сводить промежуточно, прежде чем идти в финальный
/// запрос как есть. Каждый заход стоит денег, а на разумных объёмах хватает
/// одного: ограничение нужно, чтобы цикл не крутился на патологическом входе.
const MAX_REDUCE_STAGES: usize = 3;

/// Инструкция для шага map. Живёт в коде, но перекрывается `[prompts].map_system`:
/// это не часть контракта парсинга, а обычный текст.
pub const DEFAULT_MAP_SYSTEM: &str = "\
Ты сжимаешь ЧАСТЬ ленты Telegram-каналов — её потом сведут с другими частями в общий дайджест.

Правила:
1. Сгруппируй материалы по темам, а не по каналам; одинаковые новости из разных каналов — один пункт.
2. В sources переноси ССЫЛКИ ИЗ ВХОДНЫХ ДАННЫХ дословно, все до одной: на следующем шаге взять их будет неоткуда.
3. Ничего не выдумывай и не выбрасывай значимое — это сжатие, а не отбор лучшего.
4. Помеченные как репост считай той же новостью, что и оригинал.
5. tldr оставь пустым: короткая выжимка делается на сведении.";

/// Пороги map-reduce.
///
/// В бою это константы выше; отдельным типом они стали, чтобы тесты могли
/// гонять ту же оркестрацию на коротких входах. Иначе проверка «сводки сводятся
/// промежуточно» стоила бы мегабайтов текста и сотни запросов к моку.
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// Больше этого — вход не идёт в один запрос.
    input: usize,
    /// Бюджет одной части на шаге map и одной группы на промежуточном сведении.
    batch: usize,
    /// Сколько промежуточных заходов допустимо.
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

/// Копия настроек без времени жизни ссылки на весь Config.
struct AnthropicSettings {
    api_key: String,
    api_url: String,
    model: String,
    effort: String,
    max_tokens: u32,
}

/// Ответ модели вместе с расходом токенов: без него ориентир «$0.6–0.8 за
/// прогон» не с чем сверить, а `/status` нечего показывать.
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
            .context("не удалось создать HTTP-клиент")?;

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

    /// Запрос прерываемый: при остановке токены потрачены зря, но в базе ничего
    /// нет и повтор безопасен. Ждать окончания нельзя — при `effort = "high"`
    /// ответ идёт минуты, никакой разумный `stop_grace_period` это не покроет.
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

    /// Вход не помещается в один запрос: сжимаем его частями, потом сводим.
    ///
    /// Это дороже одного запроса (N+1 вызовов), но альтернатива — отрезать
    /// хвост, а этого делать нельзя. Ссылки на посты переносятся через сводки,
    /// так что итоговая проверка `sources[].url` продолжает работать.
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
            "вход большой: сжимаем частями, затем сводим"
        );

        let map_system = prompts.map_system.as_deref().unwrap_or(DEFAULT_MAP_SYSTEM);
        let mut partials = Vec::with_capacity(batches.len());
        let mut input_tokens = None;
        let mut output_tokens = None;

        for (index, batch) in batches.iter().enumerate() {
            // В части каналов меньше, чем в прогоне: подставляем её счёт, иначе
            // промпт обещает модели материалы, которых в этой части нет.
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
                    format!("не удалось сжать часть {} из {}", index + 1, batches.len())
                })?;

            tracing::info!(
                part = index + 1,
                of = batches.len(),
                posts = batch.len(),
                topics = summary.digest.topics.len(),
                "часть сжата"
            );
            input_tokens = add(input_tokens, summary.input_tokens);
            output_tokens = add(output_tokens, summary.output_tokens);
            partials.push(summary.digest);
        }

        // Сводки тоже могут не влезть в один запрос: на сотне каналов их сумма
        // снова выходит за порог, и финальный вызов упал бы уже после того, как
        // все map-запросы оплачены. Поэтому сводим в несколько заходов.
        for stage in 1..=self.limits.stages {
            let groups = match plan_reduce(&partials, stage, self.limits) {
                ReduceStep::Ready => break,
                ReduceStep::Stuck => {
                    tracing::warn!(
                        stage,
                        partials = partials.len(),
                        chars = digest::partials_size(&partials),
                        "сводки не дробятся дальше"
                    );
                    break;
                }
                ReduceStep::Compress(groups) => groups,
            };

            tracing::info!(
                stage,
                from = partials.len(),
                to = groups.len(),
                "сводки не влезают в один запрос, сводим промежуточно"
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
                            "не удалось свести группу {} из {} на этапе {stage}",
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
        // Последняя проверка — по фактическому промпту, а не по сводкам:
        // сюда приводят и «не дробится», и исчерпанные заходы, и просто
        // большой шаблон. Отправить заведомо не влезающий запрос значит
        // потерять и эти токены тоже.
        ensure_fits(&reduce_prompt, partials.len(), self.limits)?;

        let mut summary = self
            .ask(&prompts.system, &reduce_prompt, cancel)
            .await
            .context("не удалось свести сводки частей в дайджест")?;

        summary.input_tokens = add(input_tokens, summary.input_tokens);
        summary.output_tokens = add(output_tokens, summary.output_tokens);
        Ok(summary)
    }

    /// Один запрос к модели. Прерываемый: при остановке токены потрачены зря,
    /// но в базе ничего нет и повтор безопасен. Ждать окончания нельзя — при
    /// `effort = "high"` ответ идёт минуты, никакой разумный `stop_grace_period`
    /// это не покроет.
    async fn ask(&self, system: &str, user: &str, cancel: &Cancel) -> Result<Summary> {
        // thinking на claude-opus-5 включён по умолчанию — параметр не передаём.
        // temperature и budget_tokens на этой модели возвращают 400.
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
            _ = cancel.cancelled() => bail!("запрос к модели прерван сигналом остановки"),
            result = self.post_with_retries(&body) => result?,
        };

        parse_summary(response)
    }

    /// Ретраим только то, что имеет смысл ретраить: 429 (по `retry-after`),
    /// 5xx и сетевые сбои. 400/401/403 — это ошибка конфигурации, повтор её
    /// не вылечит, падаем сразу с текстом от API.
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
                    tracing::warn!(error = %e, attempt, ?delay, "сетевая ошибка, повтор");
                    tokio::time::sleep(delay).await;
                    continue;
                }
                Err(e) => return Err(e).context("запрос к Anthropic API не удался"),
            };

            let status = response.status();

            if status.is_success() {
                return response
                    .json()
                    .await
                    .context("ответ Anthropic API — не JSON");
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
                tracing::warn!(%status, attempt, ?delay, "API вернул временную ошибку, повтор");
                tokio::time::sleep(delay).await;
                continue;
            }

            bail!("Anthropic API вернул {status}: {text}");
        }
    }
}

/// Что делать со сводками перед финальным сведением.
#[derive(Debug)]
enum ReduceStep<'a> {
    /// Влезают в один запрос — сводим.
    Ready,
    /// Ещё один промежуточный заход такими группами.
    Compress(Vec<&'a [Digest]>),
    /// Дробить дальше нечем или заходы исчерпаны: решает `ensure_fits`.
    Stuck,
}

/// Чистое решение одного шага сведения — без сети, поэтому проверяемое.
fn plan_reduce(partials: &[Digest], stage: usize, limits: Limits) -> ReduceStep<'_> {
    if digest::partials_size(partials) <= limits.input {
        return ReduceStep::Ready;
    }
    if stage > limits.stages || partials.len() < 2 {
        return ReduceStep::Stuck;
    }

    let groups = digest::split_partials(partials, limits.batch);
    // Групп столько же, сколько сводок, — заход ничего не сожмёт, только
    // потратит по запросу на каждую сводку.
    if groups.len() >= partials.len() {
        return ReduceStep::Stuck;
    }
    ReduceStep::Compress(groups)
}

/// Финальный запрос обязан помещаться в окно.
///
/// Если нет — честнее упасть, чем отправлять заведомо негодный запрос или
/// выбрасывать темы: курсоры не сдвинуты, посты останутся на следующий прогон,
/// а владелец увидит, что именно упёрлось.
fn ensure_fits(prompt: &str, partials: usize, limits: Limits) -> Result<()> {
    let chars = prompt.chars().count();
    if chars <= limits.input {
        return Ok(());
    }
    bail!(
        "сводки {partials} частей не помещаются в один запрос: {chars} символов при пороге {}. \
         Курсоры не сдвинуты, посты войдут в следующий прогон; \
         уменьшите lookback_hours или число каналов",
        limits.input
    )
}

/// Сколько разных каналов в части. Посты идут подряд по каналам, поэтому
/// достаточно посчитать смены `channel_id`.
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

/// Сумма расхода токенов по всем запросам прогона. `None` только если ни один
/// ответ счётчиков не прислал: иначе показывали бы прочерк там, где расход был.
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

/// `stop_reason` проверяем ДО чтения `content`: на отказ модель отвечает
/// HTTP 200 с `stop_reason: "refusal"` и пустым содержимым.
fn parse_summary(response: Value) -> Result<Summary> {
    let stop_reason = response
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or("");

    if stop_reason == "refusal" {
        let details = response.get("stop_details").cloned().unwrap_or(Value::Null);
        bail!("модель отклонила запрос (stop_reason=refusal): {details}");
    }
    if stop_reason == "max_tokens" {
        bail!("ответ обрезан по max_tokens — увеличьте anthropic.max_tokens в конфиге");
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
        bail!("ответ Anthropic API не содержит текстовых блоков: {response}");
    }

    let digest: Digest = serde_json::from_str(&text)
        .with_context(|| format!("не удалось разобрать ответ модели как дайджест: {text}"))?;

    tracing::info!(
        input_tokens = usage("input_tokens"),
        output_tokens = usage("output_tokens"),
        %stop_reason,
        topics = digest.topics.len(),
        "ответ модели получен"
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
            title: format!("Канал {channel_id}"),
            date: Utc::now(),
            text: "текст".into(),
            url: format!("https://t.me/c{channel_id}/1"),
            origin: None,
        }
    }

    /// Ответ Anthropic с готовым дайджестом внутри текстового блока.
    fn api_reply(summary_chars: usize, topics: usize) -> Value {
        let digest = json!({
            "topics": (0..topics).map(|i| json!({
                "title": format!("тема {i}"),
                "summary": "я".repeat(summary_chars),
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

    /// Поддельный Anthropic на localhost: считает запросы и отвечает заранее
    /// заготовленным. Нужен, чтобы проверить саму оркестрацию map-reduce —
    /// сколько запросов ушло и ушёл ли финальный вообще.
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

                // Запрос вычитываем целиком: иначе клиент получит обрыв
                // на записи тела вместо ответа.
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
            system: "сведи".into(),
            user_template: "{{post_count}} постов\n{{posts}}".into(),
            map_system: None,
            digest_title: "Дайджест за {{date}}".into(),
            tldr_title: "Коротко".into(),
        }
    }

    /// Вход, который гарантированно уходит в map-reduce: `channels` каналов
    /// по `per_channel` постов, каждый пост на `chars` символов.
    fn big_input(channels: i64, per_channel: usize, chars: usize) -> Vec<Post> {
        let mut posts = Vec::new();
        for channel in 1..=channels {
            for index in 0..per_channel {
                posts.push(Post {
                    channel_id: channel,
                    channel: format!("@c{channel}"),
                    title: format!("Канал {channel}"),
                    date: Utc::now(),
                    text: "я".repeat(chars),
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
                title: "тема".into(),
                summary: "я".repeat(summary_chars),
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
        // Восемь сводок по ~60k: вместе 480k, но по три в группу влезают.
        let partials: Vec<Digest> = (0..8).map(|_| partial(60_000)).collect();
        match plan_reduce(&partials, 1, Limits::default()) {
            ReduceStep::Compress(groups) => {
                assert!(groups.len() < partials.len());
                assert!(groups
                    .iter()
                    .all(|g| digest::partials_size(g) <= MAP_BATCH_CHARS));
            }
            other => panic!("ожидали промежуточное сведение, получили {other:?}"),
        }
    }

    #[test]
    fn partials_that_cannot_be_paired_stop_the_loop() {
        // Четыре сводки по 120k: каждая влезает в бюджет, но даже две вместе —
        // уже нет. Группировка ничего не даст, крутить заходы бессмысленно.
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
        assert!(ensure_fits("коротко", 2, Limits::default()).is_ok());

        let error = ensure_fits(&"я".repeat(LARGE_INPUT_CHARS + 1), 4, Limits::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("не помещаются в один запрос"), "{error}");
        assert!(error.contains("Курсоры не сдвинуты"), "{error}");
    }

    #[tokio::test]
    async fn map_reduce_sends_one_request_per_part_plus_the_reduce() {
        // 3 канала по 180k символов: в один запрос не влезает, в три — да.
        let posts = big_input(3, 45, 3_800);
        let api = fake_api(vec![api_reply(100, 2)]).await;

        let summary = summarizer(&api.url)
            .summarize(&prompts(), &posts, 24, 3, chrono_tz::UTC, &Cancel::never())
            .await
            .unwrap();

        assert_eq!(
            api.requests.load(Ordering::SeqCst),
            4,
            "три сжатия и одно сведение"
        );
        assert_eq!(summary.digest.topics.len(), 2);
        // Токены суммируются по всем запросам, а не берутся от последнего.
        assert_eq!(summary.input_tokens, Some(40));
        assert_eq!(summary.output_tokens, Some(12));
    }

    #[tokio::test]
    async fn an_unshrinkable_reduce_fails_before_spending_the_final_request() {
        // Каждая сводка сама по 150k: пары не складываются, промежуточное
        // сведение не поможет.
        let posts = big_input(3, 45, 3_800);
        let api = fake_api(vec![api_reply(150_000, 1)]).await;

        let error = summarizer(&api.url)
            .summarize(&prompts(), &posts, 24, 3, chrono_tz::UTC, &Cancel::never())
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("не помещаются в один запрос"), "{error}");
        assert_eq!(
            api.requests.load(Ordering::SeqCst),
            3,
            "финальный запрос не отправляется, раз он заведомо не влезет"
        );
    }

    /// Уменьшенные пороги: та же оркестрация, но вход измеряется сотнями
    /// символов, а не сотнями тысяч. Соотношение `input = 2 × batch` — как
    /// в бою, иначе тест проверял бы не ту арифметику.
    fn tiny_limits(stages: usize) -> Limits {
        Limits {
            input: 2_000,
            batch: 1_000,
            stages,
        }
    }

    /// Сводка, которую вернёт мок на такой ответ: считаем ожидания тем же
    /// кодом, что и прод, а не арифметикой в уме.
    fn partial_from(reply: &Value) -> Digest {
        parse_summary(reply.clone()).unwrap().digest
    }

    #[tokio::test]
    async fn partials_that_do_not_fit_are_compressed_before_the_final_reduce() {
        // Шесть каналов по два поста: каждый канал — своя часть (бюджет 1000).
        let posts = big_input(6, 2, 240);
        let map_reply = api_reply(400, 1);
        let limits = tiny_limits(3);

        // Предпосылки теста: сводки шести частей вместе не влезают, но
        // группируются — значит промежуточный заход обязан состояться.
        let partials: Vec<Digest> = (0..6).map(|_| partial_from(&map_reply)).collect();
        assert!(digest::partials_size(&partials) > limits.input);
        let groups = digest::split_partials(&partials, limits.batch).len();
        assert!(groups < partials.len());

        // Первые шесть ответов — крупные сводки, дальше мок отдаёт короткую:
        // после промежуточного захода всё влезает в финальный запрос.
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
            "шесть сжатий, промежуточные группы и одно сведение"
        );
        assert_eq!(summary.digest.topics.len(), 1);
        assert_eq!(
            summary.input_tokens,
            Some(10 * requests as u64),
            "токены считаются по всем запросам, включая промежуточные"
        );
    }

    #[tokio::test]
    async fn stages_run_out_and_the_final_prompt_is_still_checked() {
        // Шестнадцать частей и ровно один разрешённый заход: после него сводок
        // всё ещё слишком много. Заходы исчерпаны — но финальная проверка
        // обязана сработать и не дать отправить негодный запрос.
        let posts = big_input(16, 2, 240);
        let map_reply = api_reply(400, 1);
        let limits = tiny_limits(1);

        let partials: Vec<Digest> = (0..16).map(|_| partial_from(&map_reply)).collect();
        let groups = digest::split_partials(&partials, limits.batch).len();
        let after_stage: Vec<Digest> = (0..groups).map(|_| partial_from(&map_reply)).collect();
        assert!(
            digest::partials_size(&after_stage) > limits.input,
            "после единственного захода сводки всё ещё не влезают"
        );

        let api = fake_api(vec![map_reply]).await;

        let error = summarizer(&api.url)
            .with_limits(limits)
            .summarize(&prompts(), &posts, 24, 16, chrono_tz::UTC, &Cancel::never())
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("не помещаются в один запрос"), "{error}");
        assert_eq!(
            api.requests.load(Ordering::SeqCst),
            16 + groups,
            "шестнадцать сжатий и один заход; финальный запрос не отправлен"
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
                { "type": "text", "text": "dr\":[\"раз\"]}" }
            ]
        })))
        .unwrap();

        assert_eq!(summary.digest.tldr, vec!["раз".to_string()]);
        assert_eq!(summary.input_tokens, Some(10));
        assert_eq!(summary.output_tokens, Some(20));
    }

    #[test]
    fn invalid_json_from_the_model_is_reported_with_its_text() {
        let error = parse_summary(response(json!({
            "stop_reason": "end_turn",
            "content": [{ "type": "text", "text": "не json" }]
        })))
        .unwrap_err()
        .to_string();
        assert!(error.contains("не json"), "{error}");
    }
}
