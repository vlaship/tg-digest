//! Один прогон: сбор → модель → публикация, и восстановление недоставленного.
//!
//! Здесь же живёт единственная блокировка на всё, что трогает курсоры и статус
//! прогона. Она берётся через `try_lock` — никто не ждёт в очереди: отложенный
//! `/reset`, сработавший через пять минут, удивит сильнее, чем отказ.

use anyhow::Result;
use chrono::{Duration as ChronoDuration, Utc};
use chrono_tz::Tz;
use grammers_client::client::Client;
use std::sync::Arc;
use tokio::sync::{Mutex, MutexGuard};

use crate::cancel::Cancel;
use crate::config::Config;
use crate::db::{self, Db, Run};
use crate::digest::{self, RenderOptions};
use crate::summarize::Summarizer;
use crate::telegram::{client as tg, folder, notify, publish};

pub const TRIGGER_CRON: &str = "cron";
pub const TRIGGER_CATCH_UP: &str = "catch_up";
pub const TRIGGER_MANUAL: &str = "manual";

/// Чем закончился триггер. Тем же текстом отвечаем на `/run`.
pub enum Outcome {
    /// Блокировка занята: прогон уже идёт.
    Busy(i64),
    /// Есть недоставленный прогон, но окно повтора вышло — ждём владельца.
    Stale(i64),
    /// Недоставленный прогон добит.
    Delivered(i64),
    /// Прогон остаётся в `publishing`: часть не ушла.
    Pending(i64, String),
    Finished {
        run: i64,
        status: String,
        posts: usize,
        topics: usize,
    },
    /// Прогон не состоялся; курсоры на месте.
    Failed(i64, String),
    /// `/abandon`: блокировка снята, курсоры не применены.
    Abandoned(i64),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Busy(id) => write!(f, "идёт прогон #{id}, повторите после"),
            Outcome::Stale(id) => write!(
                f,
                "есть недоставленный прогон #{id}, окно автоповтора вышло: /resume {id} или /abandon {id}"
            ),
            Outcome::Delivered(id) => write!(f, "прогон #{id} доставлен"),
            Outcome::Pending(id, reason) => write!(f, "прогон #{id} не доставлен: {reason}"),
            Outcome::Finished {
                run,
                status,
                posts,
                topics,
            } => write!(
                f,
                "прогон #{run}: {status}, постов {posts}, тем {topics}"
            ),
            Outcome::Failed(id, reason) => write!(f, "прогон #{id} упал: {reason}"),
            Outcome::Abandoned(id) => write!(
                f,
                "прогон #{id} снят, курсоры не сдвинуты — посты войдут в следующий дайджест"
            ),
        }
    }
}

pub struct Runner {
    pub cfg: Arc<Config>,
    pub db: Arc<Db>,
    pub client: Client,
    cancel: Cancel,
    /// Одна блокировка на прогон и на команды, которые трогают курсоры.
    /// Заменяет lock-файл: процесс в контейнере один.
    lock: Mutex<()>,
    /// Последняя жалоба на папку — чтобы не слать её в «Избранное» каждый час.
    folder_warning: Mutex<Option<String>>,
}

impl Runner {
    pub fn new(cfg: Arc<Config>, db: Arc<Db>, client: Client, cancel: Cancel) -> Self {
        Self {
            cfg,
            db,
            client,
            cancel,
            lock: Mutex::new(()),
            folder_warning: Mutex::new(None),
        }
    }

    /// Блокировка для команд, которые трогают курсоры и статус прогона.
    pub fn try_lock(&self) -> Option<MutexGuard<'_, ()>> {
        self.lock.try_lock().ok()
    }

    pub fn timezone(&self) -> Tz {
        self.cfg.schedule.timezone.parse().unwrap_or(chrono_tz::UTC)
    }

    pub fn cancellation(&self) -> &Cancel {
        &self.cancel
    }

    /// Точка входа любого триггера: cron, догон, `/run`, старт демона.
    pub async fn trigger(&self, trigger: &str) -> Result<Outcome> {
        let Some(_guard) = self.try_lock() else {
            let busy = self.db.last_run().await?.map(|r| r.id).unwrap_or_default();
            return Ok(Outcome::Busy(busy));
        };

        // Проверка незавершённого прогона выполняется на КАЖДОМ триггере, а не
        // только при запуске: иначе второй дайджест уедет поверх первого.
        if let Some(run) = self.db.unfinished_run().await? {
            return self.finish_unfinished(run).await;
        }

        self.fresh_run(trigger).await
    }

    /// `/resume`: владелец принимает риск дубля. Доставка идёт с сохранёнными
    /// `random_id` — лучший шанс на дедупликацию, если срок ещё не вышел.
    pub async fn resume(&self, run_id: i64) -> Result<Outcome> {
        let Some(_guard) = self.try_lock() else {
            return Ok(Outcome::Busy(self.running_id().await?));
        };
        let Some(run) = self.db.run(run_id).await? else {
            return Ok(Outcome::Failed(run_id, "такого прогона нет".into()));
        };
        if !run.is_unfinished() {
            return Ok(Outcome::Failed(
                run_id,
                format!("прогон в статусе {}, доставлять нечего", run.status),
            ));
        }
        self.deliver(&run, true).await
    }

    pub async fn abandon(&self, run_id: i64) -> Result<Outcome> {
        let Some(_guard) = self.try_lock() else {
            return Ok(Outcome::Busy(self.running_id().await?));
        };
        let Some(run) = self.db.run(run_id).await? else {
            return Ok(Outcome::Failed(run_id, "такого прогона нет".into()));
        };
        if !run.is_unfinished() {
            return Ok(Outcome::Failed(
                run_id,
                format!("прогон в статусе {}, снимать нечего", run.status),
            ));
        }
        self.db.abandon_run(run_id).await?;
        Ok(Outcome::Abandoned(run_id))
    }

    /// Номер прогона, который сейчас держит блокировку: последний в журнале.
    async fn running_id(&self) -> Result<i64> {
        Ok(self.db.last_run().await?.map(|r| r.id).unwrap_or_default())
    }

    /// Что делать с найденным незавершённым прогоном, решает его возраст.
    async fn finish_unfinished(&self, run: Run) -> Result<Outcome> {
        let window = ChronoDuration::hours(self.cfg.telegram.publish_retry_window_hours);
        let age = Utc::now() - run.age_reference();

        if age <= window {
            tracing::info!(
                run = run.id,
                "недоставленный прогон в окне повтора, добиваем"
            );
            return self.deliver(&run, false).await;
        }

        // За пределами окна автоповтор не выполняется: срок дедупликации у
        // Telegram не документирован, рискнуть может только владелец.
        if run.stale_notified_at.is_none() {
            let text = format!(
                "прогон #{} не доставлен с {}, безопасность повтора не гарантирована:\n\
                 /resume {} — доставить недоставленные части (возможен дубль),\n\
                 /abandon {} — снять",
                run.id,
                run.age_reference().format("%Y-%m-%d %H:%M UTC"),
                run.id,
                run.id
            );
            notify(&self.client, &text).await;
            self.db.mark_stale_notified(run.id).await?;
        }
        tracing::warn!(
            run = run.id,
            "недоставленный прогон устарел, ждём решения владельца"
        );
        Ok(Outcome::Stale(run.id))
    }

    async fn deliver(&self, run: &Run, resume: bool) -> Result<Outcome> {
        let window =
            (!resume).then(|| ChronoDuration::hours(self.cfg.telegram.publish_retry_window_hours));
        match publish::deliver_pending(&self.client, &self.db, run, &self.cancel, window).await? {
            None => {
                self.db.complete_run(run.id, db::STATUS_OK).await?;
                Ok(Outcome::Delivered(run.id))
            }
            Some(reason) => {
                if !self.cancel.is_cancelled() {
                    notify(
                        &self.client,
                        &format!("дайджест не доставлен целиком: {reason}"),
                    )
                    .await;
                }
                Ok(Outcome::Pending(run.id, reason))
            }
        }
    }

    async fn fresh_run(&self, trigger: &str) -> Result<Outcome> {
        self.refresh_channels().await?;

        let channels = self.db.enabled_channels().await?;
        let run_id = self.db.start_run(trigger).await?;
        tracing::info!(run = run_id, %trigger, channels = channels.len(), "прогон начат");

        if channels.is_empty() {
            tracing::warn!("включённых каналов нет — публиковать нечего");
            self.db.complete_run(run_id, db::STATUS_EMPTY).await?;
            return Ok(Outcome::Finished {
                run: run_id,
                status: db::STATUS_EMPTY.into(),
                posts: 0,
                topics: 0,
            });
        }

        let collected =
            match tg::collect_posts(&self.client, &self.cfg, &channels, &self.cancel).await {
                Ok(collected) => collected,
                Err(e) => return self.fail(run_id, e).await,
            };

        // Исключённый канал — повод для уведомления: молча потерять его посты
        // нельзя, а курсор у него не двигается и следующий прогон дочитает.
        let failed: Vec<i64> = collected.failures.iter().map(|(c, _)| c.id).collect();
        for (channel, reason) in &collected.failures {
            self.db.set_channel_error(channel.id, Some(reason)).await?;
            notify(
                &self.client,
                &format!("канал {} исключён из прогона: {reason}", channel.label()),
            )
            .await;
        }
        for channel in channels.iter().filter(|c| !failed.contains(&c.id)) {
            if channel.last_error.is_some() {
                self.db.set_channel_error(channel.id, None).await?;
            }
        }

        if collected.posts.is_empty() {
            tracing::info!(run = run_id, "новых постов нет");
            return self.finish_empty(run_id, &collected, (None, None)).await;
        }

        let summarizer = match Summarizer::new(&self.cfg.anthropic) {
            Ok(s) => s,
            Err(e) => return self.fail(run_id, e).await,
        };
        let summary = match summarizer
            .summarize(
                &self.cfg.prompts,
                &collected.posts,
                self.cfg.telegram.lookback_hours,
                channels.len() - failed.len(),
                self.timezone(),
                &self.cancel,
            )
            .await
        {
            Ok(summary) => summary,
            Err(e) => return self.fail(run_id, e).await,
        };

        let mut digest = summary.digest;
        digest::drop_foreign_sources(&mut digest, &collected.posts);

        let chunks = digest::render_html(
            &digest,
            &RenderOptions {
                digest_title: &self.cfg.prompts.digest_title,
                tldr_title: &self.cfg.prompts.tldr_title,
                timezone: self.timezone(),
                now: Utc::now(),
            },
        );

        // Пустой дайджест не публикуем, но посты прочитаны: курсоры двигаем,
        // иначе «неважные» посты перечитывались бы каждый прогон.
        if chunks.is_empty() {
            tracing::info!(run = run_id, "модель не нашла ни одной темы");
            return self
                .finish_empty(
                    run_id,
                    &collected,
                    (summary.input_tokens, summary.output_tokens),
                )
                .await;
        }

        let dest_label = self.cfg.telegram.destination.clone();
        let dest = match publish::resolve_destination(&self.client, &dest_label).await {
            Ok(dest) => dest,
            Err(e) => return self.fail(run_id, e).await,
        };

        let journal: Vec<(String, i64)> = chunks
            .iter()
            .map(|html| (html.clone(), rand::random()))
            .collect();

        self.db
            .save_summary(
                run_id,
                collected.posts.len(),
                digest.topics.len(),
                (summary.input_tokens, summary.output_tokens),
                &collected.cursors,
                &dest_label,
                dest,
                &journal,
            )
            .await?;

        let run = self
            .db
            .run(run_id)
            .await?
            .expect("только что записанный прогон");

        match self.deliver(&run, false).await? {
            Outcome::Delivered(_) => Ok(Outcome::Finished {
                run: run_id,
                status: db::STATUS_OK.into(),
                posts: collected.posts.len(),
                topics: digest.topics.len(),
            }),
            other => Ok(other),
        }
    }

    async fn finish_empty(
        &self,
        run_id: i64,
        collected: &tg::Collected,
        usage: (Option<u64>, Option<u64>),
    ) -> Result<Outcome> {
        self.db
            .save_empty_cursors(run_id, &collected.cursors, collected.posts.len(), usage)
            .await?;
        self.db.complete_run(run_id, db::STATUS_EMPTY).await?;
        Ok(Outcome::Finished {
            run: run_id,
            status: db::STATUS_EMPTY.into(),
            posts: collected.posts.len(),
            topics: 0,
        })
    }

    /// Прогон не дошёл до ответа модели: ничего не записано, курсоры целы.
    async fn fail(&self, run_id: i64, error: anyhow::Error) -> Result<Outcome> {
        if tg::is_fatal(&error) {
            // Отозванную сессию пережидать бессмысленно — пусть падает наружу.
            self.db.fail_run(run_id, &format!("{error:#}")).await?;
            return Err(error);
        }

        let reason = if self.cancel.is_cancelled() {
            "cancelled".to_string()
        } else {
            format!("{error:#}")
        };

        tracing::error!(run = run_id, %reason, "прогон завершился с ошибкой");
        self.db.fail_run(run_id, &reason).await?;

        if !self.cancel.is_cancelled() {
            notify(&self.client, &format!("дайджест не собран: {reason}")).await;
        }
        Ok(Outcome::Failed(run_id, reason))
    }

    /// Привести базу в соответствие с Telegram перед сбором.
    ///
    /// С папкой это полный sync. Без папки составом управляют `/add` и `/rm`,
    /// но названия и username всё равно надо перечитать: переименованный канал
    /// иначе навсегда остался бы в дайджесте со старой ссылкой.
    pub async fn refresh_channels(&self) -> Result<()> {
        if self.cfg.telegram.folder.is_some() {
            self.sync_folder().await?;
            return Ok(());
        }

        match folder::refresh_names(&self.client, &self.db).await {
            Ok(_) => Ok(()),
            Err(e) if tg::is_fatal(&e) => Err(e),
            // Не смогли перечитать имена — это не повод отменять прогон:
            // ссылки останутся прежними, дайджест соберётся.
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "не удалось обновить названия каналов");
                Ok(())
            }
        }
    }

    /// Папка — единый источник правды по каналам, но её проблемы не должны
    /// ронять прогон: не синхронизировались — работаем по базе.
    pub async fn sync_folder(&self) -> Result<Option<folder::SyncReport>> {
        let Some(title) = self.cfg.telegram.folder.as_deref() else {
            return Ok(None);
        };

        let report = match folder::sync(&self.client, &self.db, title).await {
            Ok(report) => report,
            Err(e) if tg::is_fatal(&e) => return Err(e),
            Err(e) => {
                let text = format!("папка {title:?} не синхронизирована: {e:#}");
                tracing::warn!("{text}");
                self.warn_once(text).await;
                return Ok(None);
            }
        };

        match &report.refused {
            Some(reason) => {
                tracing::warn!("{reason}");
                self.warn_once(reason.clone()).await;
            }
            None => *self.folder_warning.lock().await = None,
        }
        Ok(Some(report))
    }

    /// Одна и та же жалоба на папку уходит в «Избранное» один раз, а не на
    /// каждом триггере.
    async fn warn_once(&self, text: String) {
        let mut last = self.folder_warning.lock().await;
        if last.as_deref() == Some(text.as_str()) {
            return;
        }
        notify(&self.client, &text).await;
        *last = Some(text);
    }
}
