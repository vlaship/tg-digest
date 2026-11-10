//! One run: collect → model → publish, and recovery of undelivered runs.
//!
//! The single lock for everything that touches cursors and run status also lives
//! here. It is taken with `try_lock` — nobody waits in a queue: a delayed
//! `/reset` that fires five minutes later is more surprising than a refusal.

use anyhow::{anyhow, Result};
use chrono::{Duration as ChronoDuration, Utc};
use chrono_tz::Tz;
use grammers_client::client::Client;
use std::sync::Arc;
use tokio::sync::{Mutex, MutexGuard};
use tracing::Instrument;

use crate::cancel::Cancel;
use crate::config::{Config, Prompts, Theme};
use crate::db::{self, Db, NewChunk, Run};
use crate::digest::{self, RenderOptions};
use crate::models::Catalog;
use crate::summarize::{ApiError, ApiErrorKind, Summarizer};
use crate::telegram::bot::Bot;
use crate::telegram::{channels, client as tg, photos, publish};

pub const TRIGGER_CRON: &str = "cron";
pub const TRIGGER_CATCH_UP: &str = "catch_up";
pub const TRIGGER_MANUAL: &str = "manual";

/// Key in the `settings` table for the model the owner chose.
const MODEL_SETTING: &str = "model";
/// Key in the `settings` table for a one-time window set by `/reread`, in hours.
/// One key per theme: it stays until a run of THAT theme reads the posts (any
/// outcome but a failure), so a failed theme keeps its window for its next run.
fn reread_setting(theme: &str) -> String {
    format!("reread_hours:{theme}")
}

/// The key of the `/reread` window from before it was per theme: one window for all.
const OLD_REREAD_SETTING: &str = "reread_hours";

/// A database from before: a `/reread` window that still waits for a run goes to
/// every theme, and the old key is deleted. Called on every start; it does nothing
/// when there is no old key.
pub async fn adopt_old_reread(db: &Db, cfg: &Config) -> Result<()> {
    let Some(hours) = db.setting(OLD_REREAD_SETTING).await? else {
        return Ok(());
    };
    for theme in cfg.themes() {
        db.set_setting(&reread_setting(&theme.tag), &hours).await?;
    }
    db.delete_setting(OLD_REREAD_SETTING).await?;
    tracing::info!(%hours, "the reread window now belongs to every theme");
    Ok(())
}

/// How a trigger ended. We reply to `/run` with the same text.
pub enum Outcome {
    /// The lock is taken: a run is already going.
    Busy(i64),
    /// There is an undelivered run, but the retry window is over — waiting for the owner.
    Stale(i64),
    /// The undelivered run is now delivered.
    Delivered(i64),
    /// The run stays in `publishing`: a part was not sent.
    Pending(i64, String),
    Finished {
        run: i64,
        status: String,
        posts: usize,
        topics: usize,
    },
    /// The run did not happen; the cursors are in place.
    Failed(i64, String),
    /// `/abandon`: the lock is released, the cursors are not applied.
    Abandoned(i64),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Busy(id) => write!(f, "run #{id} is going, try again later"),
            Outcome::Stale(id) => write!(
                f,
                "there is an undelivered run #{id}, the auto-retry window is over: /resume {id} or /abandon {id}"
            ),
            Outcome::Delivered(id) => write!(f, "run #{id} delivered"),
            Outcome::Pending(id, reason) => write!(f, "run #{id} not delivered: {reason}"),
            Outcome::Finished {
                run,
                status,
                posts,
                topics,
            } => write!(
                f,
                "run #{run}: {status}, posts {posts}, topics {topics}"
            ),
            Outcome::Failed(id, reason) => write!(f, "run #{id}: {reason}"),
            Outcome::Abandoned(id) => write!(
                f,
                "run #{id} cancelled, cursors not moved — the posts go into the next digest"
            ),
        }
    }
}

/// What each theme did on one trigger, in the order of the themes. The tag is
/// empty when there is nothing to tell apart: one theme, or the lock is taken.
pub struct Outcomes(pub Vec<(String, Outcome)>);

impl std::fmt::Display for Outcomes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            return write!(f, "nothing to do");
        }
        for (i, (tag, outcome)) in self.0.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            if tag.is_empty() {
                write!(f, "{outcome}")?;
            } else {
                write!(f, "{tag}: {outcome}")?;
            }
        }
        Ok(())
    }
}

pub struct Runner {
    pub cfg: Arc<Config>,
    pub db: Arc<Db>,
    /// The personal account: reads the channels.
    pub client: Client,
    /// The bot: posts digests and notifications. `None` in the debug commands
    /// (`preview`, `run --dry-run`): they never send anything.
    pub bot: Option<Bot>,
    /// The models of each provider, read from their APIs.
    pub catalog: Catalog,
    cancel: Cancel,
    /// One lock for the run and for commands that touch cursors.
    /// It replaces a lock file: there is one process in the container.
    lock: Mutex<()>,
}

impl Runner {
    pub fn new(
        cfg: Arc<Config>,
        db: Arc<Db>,
        client: Client,
        bot: Option<Bot>,
        cancel: Cancel,
    ) -> Self {
        Self {
            catalog: Catalog::new(cfg.clone(), db.clone()),
            cfg,
            db,
            client,
            bot,
            cancel,
            lock: Mutex::new(()),
        }
    }

    /// The bot, for everything that sends. Only `serve` has one.
    pub fn bot(&self) -> Result<&Bot> {
        self.bot
            .as_ref()
            .ok_or_else(|| anyhow!("sending needs the bot: only `serve` sends"))
    }

    /// A notification to the owner through the bot. Without a bot it only goes to the log.
    pub async fn notify(&self, text: &str) {
        match &self.bot {
            Some(bot) => bot.notify(text).await,
            None => tracing::warn!(%text, "notification (no bot in this command)"),
        }
    }

    /// The model for the next run, as `provider/model`: the one the owner chose with
    /// `/model` or `/fallback` (kept in the database), else `model` from the config.
    /// A saved model whose provider is no longer in the config is ignored.
    pub async fn model(&self) -> String {
        match self.db.setting(MODEL_SETTING).await {
            Ok(Some(key)) if self.cfg.model_id(&key).is_ok() => key,
            Ok(_) => self.cfg.model.clone(),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "could not read the model setting");
                self.cfg.model.clone()
            }
        }
    }

    /// Switch the model. The key and the provider's model list are checked first.
    pub async fn set_model(&self, key: &str) -> Result<()> {
        self.catalog.spec(key).await?;
        self.db.set_setting(MODEL_SETTING, key).await?;
        tracing::info!(model = key, "model switched");
        Ok(())
    }

    /// Where `/fallback` goes from the current model: the first model in the
    /// `fallback` list that is not the current one.
    pub async fn next_fallback(&self) -> Option<String> {
        let current = self.model().await;
        self.cfg.fallback.iter().find(|m| **m != current).cloned()
    }

    /// The lock for commands that touch cursors and run status.
    pub fn try_lock(&self) -> Option<MutexGuard<'_, ()>> {
        self.lock.try_lock().ok()
    }

    pub fn timezone(&self) -> Tz {
        self.cfg.schedule.timezone.parse().unwrap_or(chrono_tz::UTC)
    }

    pub fn cancellation(&self) -> &Cancel {
        &self.cancel
    }

    /// Entry point of every trigger: cron, catch-up, `/run`, daemon start. The themes
    /// run one after another and do not depend on each other: each one has its own
    /// channels, cursors and runs. `only` limits the trigger to one theme.
    pub async fn trigger(&self, trigger: &str, only: Option<&Theme>) -> Result<Outcomes> {
        self.pass(trigger, &self.themes(only), true).await
    }

    /// A trigger for a chosen set of themes in ONE pass: the names are refreshed
    /// once and the `/run` tap comes once at the end. For the daemon start, where
    /// only some themes have to run.
    pub async fn trigger_themes(&self, trigger: &str, themes: &[Theme]) -> Result<Outcomes> {
        self.pass(trigger, themes, true).await
    }

    fn themes(&self, only: Option<&Theme>) -> Vec<Theme> {
        match only {
            Some(theme) => vec![theme.clone()],
            None => self.cfg.themes(),
        }
    }

    /// Only deliver the undelivered runs, start nothing new: for the daemon start.
    pub async fn recover(&self) -> Result<Outcomes> {
        self.pass(TRIGGER_CATCH_UP, &self.cfg.themes(), false).await
    }

    async fn pass(&self, trigger: &str, themes: &[Theme], fresh: bool) -> Result<Outcomes> {
        let Some(_guard) = self.try_lock() else {
            let busy = Outcome::Busy(self.running_id().await?);
            return Ok(Outcomes(vec![(String::new(), busy)]));
        };
        self.pass_locked(trigger, themes, fresh).await
    }

    /// The same as `pass`, for a caller that already holds the lock.
    async fn pass_locked(&self, trigger: &str, themes: &[Theme], fresh: bool) -> Result<Outcomes> {
        let named = self.cfg.themes().len() > 1;
        let (mut refreshed, mut offer) = (false, false);
        let mut out = Vec::new();

        for theme in themes {
            if self.cancel.is_cancelled() {
                break;
            }
            // The check for an unfinished run happens on EVERY trigger, not only
            // on start: otherwise a second digest would go on top of the first one.
            let unfinished = self.db.unfinished_theme_run(&theme.tag).await?;
            let is_fresh = unfinished.is_none();
            // Every log line of the work on a theme starts with `run{theme=…}`, so
            // the log shows which theme the collecting, the model and the sending
            // belong to.
            let span = theme_span(&theme.tag);
            let result = match unfinished {
                Some(run) => self.finish_unfinished(run).instrument(span.clone()).await,
                None if !fresh => continue,
                None => {
                    if !refreshed {
                        self.refresh_channels().await?;
                        refreshed = true;
                    }
                    self.fresh_run(trigger, theme)
                        .instrument(span.clone())
                        .await
                }
            };
            let outcome = match result {
                Ok(outcome) => outcome,
                Err(e) if tg::is_fatal(&e) => return Err(e),
                // An error in one theme must not stop the other themes.
                Err(e) => self.broken(trigger, theme, e).instrument(span).await,
            };

            // The `/reread` window of the theme is used up once the theme has read
            // its posts. A failed theme keeps it for its next run.
            if is_fresh && !matches!(outcome, Outcome::Failed(..)) {
                let key = reread_setting(&theme.tag);
                if let Err(e) = self.db.delete_setting(&key).await {
                    tracing::warn!(error = %format!("{e:#}"), "could not clear the reread window");
                }
            }
            let delivered = match &outcome {
                Outcome::Delivered(_) => true,
                Outcome::Finished { status, .. } => status == db::STATUS_OK,
                _ => false,
            };
            offer |= delivered && theme.to_bot();
            let tag = if named {
                theme.tag.clone()
            } else {
                String::new()
            };
            out.push((tag, outcome));
        }

        // After a digest in the chat with the bot, one more message with `/run`: the
        // owner taps it to build the next digest at once. Not for a channel: nobody
        // there can send commands.
        if offer {
            self.notify("/run").await;
        }
        Ok(Outcomes(out))
    }

    /// A theme failed with an error that is not a normal run failure (the database,
    /// for example). We tell the owner and go on with the next theme.
    async fn broken(&self, trigger: &str, theme: &Theme, error: anyhow::Error) -> Outcome {
        let reason = format!("{error:#}");
        tracing::error!(%trigger, %reason, "run failed");
        let short: String = reason.chars().take(300).collect();
        let text = format!("⚠️ The run failed: {short}");

        // A run that broke while collecting must not stay `collecting` until the next
        // restart. A run that is `summarized` or `publishing` is kept: it has its
        // parts and is delivered later.
        let mut run_id = self.running_id().await.unwrap_or_default();
        match self.db.last_theme_run(&theme.tag).await {
            Ok(Some(run)) if run.status == db::STATUS_COLLECTING => {
                run_id = run.id;
                if let Err(e) = self.db.fail_run(run.id, &reason).await {
                    tracing::warn!(error = %format!("{e:#}"), run = run.id, "could not mark the run as failed");
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "could not read the run of the theme")
            }
        }

        if trigger != TRIGGER_MANUAL {
            self.notify(&self.named(&theme.tag, &text)).await;
        }
        Outcome::Failed(run_id, text)
    }

    /// A notification text with the theme tag in front, when there are several themes.
    fn named(&self, tag: &str, text: &str) -> String {
        if self.cfg.themes().len() > 1 {
            format!("[{tag}] {text}")
        } else {
            text.to_string()
        }
    }

    /// `/reread N`: read all channels again for the last N days (of one theme, or of
    /// all), then run at once. The cursors go to 0, and the next run of each theme
    /// uses a window of N days instead of `lookback_hours`. One lock covers both
    /// steps, so a scheduled run cannot start between them. The `Err` is the
    /// refusal text: a run is going, or a theme has an undelivered run.
    pub async fn reread(
        &self,
        days: i64,
        only: Option<&Theme>,
    ) -> Result<std::result::Result<(usize, Outcomes), String>> {
        let Some(_guard) = self.try_lock() else {
            return Ok(Err(Outcome::Busy(self.running_id().await?).to_string()));
        };
        let themes = self.themes(only);
        for theme in &themes {
            if let Some(refusal) = self.undelivered_refusal(Some(&theme.tag)).await? {
                return Ok(Err(refusal));
            }
        }

        let mut count = 0;
        for theme in &themes {
            for channel in self.db.theme_channels(&theme.tag).await? {
                self.db.reset_channel(&channel.theme, channel.id).await?;
                count += 1;
            }
            self.db
                .set_setting(&reread_setting(&theme.tag), &(days * 24).to_string())
                .await?;
        }
        tracing::info!(days, channels = count, "reread prepared");

        let outcomes = self.pass_locked(TRIGGER_MANUAL, &themes, true).await?;
        Ok(Ok((count, outcomes)))
    }

    /// The refusal for a command that touches cursors while a run is undelivered:
    /// delivery would write the run's saved cursors over the change. Only the theme
    /// of the command counts (`None`: any theme); a run of one theme never blocks
    /// another theme.
    pub async fn undelivered_refusal(&self, theme: Option<&str>) -> Result<Option<String>> {
        let run = match theme {
            Some(theme) => self.db.unfinished_theme_run(theme).await?,
            None => self.db.unfinished_run().await?,
        };
        Ok(run.map(|run| {
            format!(
                "there is an undelivered run #{} ({}) — /run to deliver it or /abandon {}",
                run.id, run.theme, run.id
            )
        }))
    }

    /// The prompts of the theme for the model. A channel often writes about
    /// yesterday's story again in a new post: tell the model what the last digest
    /// already had. Not on `/reread`: the owner asked for these days again, the old
    /// topics would be dropped. `run --dry-run` uses this too, so it sees the same.
    pub async fn theme_prompts(&self, theme: &Theme, reread: bool) -> Prompts {
        let previous = if reread {
            Vec::new()
        } else {
            self.db.last_digest_topics(&theme.tag).await.unwrap_or_else(|e| {
                tracing::warn!(error = %format!("{e:#}"), "could not read the last digest topics");
                Vec::new()
            })
        };
        with_previous_topics(&theme.prompts, &previous)
    }

    /// The window of the next run of the theme, in hours: from `/reread`, else
    /// `lookback_hours`. The flag says it is a `/reread` window.
    pub async fn lookback_hours(&self, theme: &str) -> (i64, bool) {
        match self.db.setting(&reread_setting(theme)).await {
            Ok(Some(hours)) => match hours.parse() {
                Ok(hours) => (hours, true),
                Err(_) => (self.cfg.telegram.lookback_hours, false),
            },
            _ => (self.cfg.telegram.lookback_hours, false),
        }
    }

    /// `/resume`: the owner accepts the risk of a duplicate. Delivery uses the saved
    /// `random_id` — the best chance of deduplication if the time has not run out yet.
    pub async fn resume(&self, run_id: i64) -> Result<Outcome> {
        let Some(_guard) = self.try_lock() else {
            return Ok(Outcome::Busy(self.running_id().await?));
        };
        match self.undelivered(run_id, "deliver").await? {
            Ok(run) => {
                let span = theme_span(&run.theme);
                self.deliver(&run, true).instrument(span).await
            }
            Err(refusal) => Ok(refusal),
        }
    }

    pub async fn abandon(&self, run_id: i64) -> Result<Outcome> {
        let Some(_guard) = self.try_lock() else {
            return Ok(Outcome::Busy(self.running_id().await?));
        };
        if let Err(refusal) = self.undelivered(run_id, "cancel").await? {
            return Ok(refusal);
        }
        self.db.abandon_run(run_id).await?;
        photos::remove(&self.photos_dir(), run_id);
        Ok(Outcome::Abandoned(run_id))
    }

    /// The undelivered run with this number, for `/resume` and `/abandon`. The inner
    /// `Err` is the reply when there is no such run or it has nothing to `verb`.
    async fn undelivered(
        &self,
        run_id: i64,
        verb: &str,
    ) -> Result<std::result::Result<Run, Outcome>> {
        let Some(run) = self.db.run(run_id).await? else {
            return Ok(Err(Outcome::Failed(run_id, "no such run".into())));
        };
        if !run.is_unfinished() {
            return Ok(Err(Outcome::Failed(
                run_id,
                format!("run has status {}, nothing to {verb}", run.status),
            )));
        }
        Ok(Ok(run))
    }

    /// Number of the run that holds the lock now: the last one in the journal.
    pub async fn running_id(&self) -> Result<i64> {
        Ok(self.db.last_run().await?.map(|r| r.id).unwrap_or_default())
    }

    /// The age of a found unfinished run decides what to do with it.
    async fn finish_unfinished(&self, run: Run) -> Result<Outcome> {
        let window = ChronoDuration::hours(self.cfg.telegram.publish_retry_window_hours);
        let age = Utc::now() - run.age_reference();

        if age <= window {
            tracing::info!(
                run = run.id,
                "undelivered run inside the retry window, delivering it"
            );
            return self.deliver(&run, false).await;
        }

        // Outside the window there is no auto-retry: Telegram does not document
        // the deduplication time, so only the owner can take the risk.
        if run.stale_notified_at.is_none() {
            let text = format!(
                "{}run #{} not delivered since {}, a retry is not guaranteed to be safe:\n\
                 /resume {} — deliver the undelivered parts (a duplicate is possible),\n\
                 /abandon {} — cancel",
                self.named(&run.theme, ""),
                run.id,
                run.age_reference().format("%Y-%m-%d %H:%M UTC"),
                run.id,
                run.id
            );
            self.notify(&text).await;
            self.db.mark_stale_notified(run.id).await?;
        }
        tracing::warn!(
            run = run.id,
            "undelivered run is too old, waiting for the owner to decide"
        );
        Ok(Outcome::Stale(run.id))
    }

    async fn deliver(&self, run: &Run, resume: bool) -> Result<Outcome> {
        let window =
            (!resume).then(|| ChronoDuration::hours(self.cfg.telegram.publish_retry_window_hours));
        let bot = self.bot()?;
        let dir = self.photos_dir();
        match publish::deliver_pending(&bot.client, &self.db, run, &dir, &self.cancel, window)
            .await?
        {
            None => {
                self.db.complete_run(run.id, db::STATUS_OK).await?;
                photos::remove(&dir, run.id);
                self.mark_read(&run.cursors).await;
                Ok(Outcome::Delivered(run.id))
            }
            Some(reason) => {
                if !self.cancel.is_cancelled() {
                    let text = format!("digest not fully delivered: {reason}");
                    self.notify(&self.named(&run.theme, &text)).await;
                }
                Ok(Outcome::Pending(run.id, reason))
            }
        }
    }

    async fn fresh_run(&self, trigger: &str, theme: &Theme) -> Result<Outcome> {
        let channels = self.db.theme_channels(&theme.tag).await?;
        let run_id = self.db.start_run(trigger, &theme.tag).await?;
        tracing::info!(run = run_id, %trigger, channels = channels.len(), "run started");

        if channels.is_empty() {
            tracing::warn!("no channels enabled — nothing to post");
            self.db.complete_run(run_id, db::STATUS_EMPTY).await?;
            return Ok(Outcome::Finished {
                run: run_id,
                status: db::STATUS_EMPTY.into(),
                posts: 0,
                topics: 0,
            });
        }

        let (lookback_hours, reread) = self.lookback_hours(&theme.tag).await;
        let collected = match tg::collect_posts(
            &self.client,
            &self.cfg,
            &channels,
            lookback_hours,
            &self.cancel,
        )
        .await
        {
            Ok(collected) => collected,
            Err(e) => return self.fail(run_id, trigger, theme, e).await,
        };

        // An excluded channel is a reason for a notification: we cannot silently lose
        // its posts, and its cursor does not move, so the next run reads the rest.
        let failed: Vec<i64> = collected.failures.iter().map(|(c, _)| c.id).collect();
        for (channel, reason) in &collected.failures {
            self.db
                .set_channel_error(&theme.tag, channel.id, Some(reason))
                .await?;
            let text = format!(
                "channel {} excluded from the run: {reason}",
                channel.label()
            );
            self.notify(&self.named(&theme.tag, &text)).await;
        }
        for channel in channels.iter().filter(|c| !failed.contains(&c.id)) {
            if channel.last_error.is_some() {
                self.db
                    .set_channel_error(&theme.tag, channel.id, None)
                    .await?;
            }
        }

        if collected.posts.is_empty() {
            tracing::info!(run = run_id, "no new posts");
            return self.finish_empty(run_id, &collected, (None, None)).await;
        }

        let summarizer = match self
            .catalog
            .spec(&self.model().await)
            .await
            .and_then(|model| Summarizer::new(&model))
        {
            Ok(s) => s,
            Err(e) => return self.fail(run_id, trigger, theme, e).await,
        };
        let prompts = self.theme_prompts(theme, reread).await;
        let summary = match summarizer
            .summarize(
                &prompts,
                &collected.posts,
                lookback_hours,
                channels.len() - failed.len(),
                self.timezone(),
                &self.cancel,
            )
            .await
        {
            Ok(summary) => summary,
            Err(e) => return self.fail(run_id, trigger, theme, e).await,
        };

        let mut digest = summary.digest;
        digest::drop_foreign_sources(&mut digest, &collected.posts);

        let chunks = digest::render_parts(
            &digest,
            &RenderOptions {
                digest_title: &theme.prompts.digest_title,
                timezone: self.timezone(),
                now: Utc::now(),
            },
        );

        // Many posts and almost no topics is a broken answer, not a quiet day (a
        // compressed model on a cheap host can do this). The run fails, and the
        // cursors stay, so the posts go into the next run.
        // It goes through `fail` like any other run error: a plain `Err` here would
        // leave the run in `collecting` and stop the daemon on a cron trigger.
        if collected.posts.len() >= MIN_POSTS_FOR_CHECK && digest.topics.len() < 2 {
            let error = anyhow!(
                "the model gave {} topic(s) for {} posts (output tokens: {}). \
                 The answer looks broken, so nothing is posted. Try again or switch \
                 the model (/model)",
                digest.topics.len(),
                collected.posts.len(),
                summary.output_tokens.unwrap_or(0)
            );
            return self.fail(run_id, trigger, theme, error).await;
        }

        // We do not post an empty digest, but the posts were read: we move the cursors,
        // or "unimportant" posts would be read again on every run.
        if chunks.is_empty() {
            tracing::info!(run = run_id, "the model found no topics");
            return self
                .finish_empty(
                    run_id,
                    &collected,
                    (summary.input_tokens, summary.output_tokens),
                )
                .await;
        }

        let dest_label = theme.destination.clone();
        let resolved = match self.bot() {
            Ok(bot) => publish::resolve_destination(bot, &dest_label).await,
            Err(e) => Err(e),
        };
        let dest = match resolved {
            Ok(dest) => dest,
            Err(e) => return self.fail(run_id, trigger, theme, e).await,
        };

        let journal = self
            .with_photos(run_id, &digest, chunks, &collected.posts)
            .await;

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
        let topics: Vec<String> = digest
            .topics
            .iter()
            .map(|t| short_topic(&t.title, &t.summary))
            .collect();
        self.db.set_digest_topics(run_id, &topics).await?;

        let run = self
            .db
            .run(run_id)
            .await?
            .expect("a run that was just written");

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

    fn photos_dir(&self) -> std::path::PathBuf {
        photos::dir(&self.cfg.db_file)
    }

    /// The journal parts with the photos of each topic. If the topic is one message and
    /// its text fits in a photo caption, it goes as one message with the photos.
    /// Otherwise the photos go first, then the text. A photo that could not be
    /// downloaded is only logged: the digest is sent without it.
    async fn with_photos(
        &self,
        run_id: i64,
        digest: &digest::Digest,
        parts: Vec<digest::Part>,
        posts: &[digest::Post],
    ) -> Vec<NewChunk> {
        let dir = self.photos_dir();
        let mut journal = Vec::with_capacity(parts.len());
        let mut done = std::collections::HashSet::new();

        for (i, part) in parts.iter().enumerate() {
            let text = NewChunk {
                html: part.html.clone(),
                random_id: rand::random(),
                photos: 0,
            };
            let Some(topic) = part.topic.filter(|t| done.insert(*t)) else {
                journal.push(text);
                continue;
            };

            let files = self.topic_photos(&digest.topics[topic], posts).await;
            if files.is_empty() {
                journal.push(text);
                continue;
            }
            if let Err(e) = photos::save(&dir, run_id, journal.len(), &files) {
                tracing::warn!(error = %format!("{e:#}"), "could not save the photos");
                journal.push(text);
                continue;
            }

            let one_message = parts.get(i + 1).map(|next| next.topic) != Some(Some(topic));
            let in_caption = one_message && publish::fits_caption(&part.html);
            tracing::info!(
                topic = %digest.topics[topic].title,
                photos = files.len(),
                kb = files.iter().map(Vec::len).sum::<usize>() / 1024,
                in_caption,
                "photos added to the topic"
            );
            if in_caption {
                journal.push(NewChunk {
                    photos: files.len(),
                    ..text
                });
            } else {
                journal.push(NewChunk {
                    html: String::new(),
                    random_id: rand::random(),
                    photos: files.len(),
                });
                journal.push(text);
            }
        }
        journal
    }

    async fn topic_photos(&self, topic: &digest::Topic, posts: &[digest::Post]) -> Vec<Vec<u8>> {
        let Some((channel_id, ids)) = digest::pick_photos(topic, posts) else {
            tracing::debug!(topic = %topic.title, "no photos in the sources of the topic");
            return Vec::new();
        };
        match photos::download(&self.client, &self.db, channel_id, &ids).await {
            Ok(files) => files,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), topic = %topic.title, "photos not added");
                Vec::new()
            }
        }
    }

    /// The cursors are applied: mark those posts as read in Telegram too
    /// (`telegram.mark_read`). Only for the personal account; errors go to the log.
    async fn mark_read(&self, cursors: &std::collections::BTreeMap<i64, i32>) {
        if self.cfg.telegram.mark_read && !cursors.is_empty() {
            channels::mark_read(&self.client, &self.db, cursors).await;
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
        self.mark_read(&collected.cursors).await;
        Ok(Outcome::Finished {
            run: run_id,
            status: db::STATUS_EMPTY.into(),
            posts: collected.posts.len(),
            topics: 0,
        })
    }

    /// The run did not reach the model answer: nothing is saved, the cursors are safe.
    ///
    /// The full error goes to the log and the database; the owner gets a short text
    /// in plain words (`friendly_error`). A `/run` gets it as the reply, so we do not
    /// send it a second time as a notification.
    async fn fail(
        &self,
        run_id: i64,
        trigger: &str,
        theme: &Theme,
        error: anyhow::Error,
    ) -> Result<Outcome> {
        if tg::is_fatal(&error) {
            // There is no point in waiting out a revoked session — let it fail upwards.
            self.db.fail_run(run_id, &format!("{error:#}")).await?;
            return Err(error);
        }

        if self.cancel.is_cancelled() {
            tracing::error!(run = run_id, "run cancelled");
            self.db.fail_run(run_id, "cancelled").await?;
            return Ok(Outcome::Failed(run_id, "cancelled".into()));
        }

        let reason = format!("{error:#}");
        tracing::error!(run = run_id, %reason, "run finished with an error");
        self.db.fail_run(run_id, &reason).await?;

        let text = self.friendly_error(&error).await;
        if trigger != TRIGGER_MANUAL {
            self.notify(&self.named(&theme.tag, &text)).await;
        }
        Ok(Outcome::Failed(run_id, text))
    }

    /// What went wrong and what to do, in plain words, without a JSON dump.
    async fn friendly_error(&self, error: &anyhow::Error) -> String {
        const KEPT: &str = "The posts are kept and go into the next run.";
        let Some(api_error) = error.chain().find_map(|e| e.downcast_ref::<ApiError>()) else {
            // Not an API error: the first line of the reason is enough, the rest is in the log.
            let reason = format!("{error:#}");
            let short: String = reason
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(300)
                .collect();
            return format!("⚠️ The digest is not built: {short}\n{KEPT}");
        };

        let api = api_error.api;
        let current = self.model().await;
        let fallback = self.next_fallback().await;
        let switch = |otherwise: &str| match &fallback {
            Some(fallback) => format!(
                "Switch to {fallback} and build the digest now? Send /fallback.\n\
                 Later, /model {current} switches back."
            ),
            None => otherwise.to_string(),
        };

        match api_error.kind {
            ApiErrorKind::Limit => format!(
                "⚠️ The {api} limit is used up, the digest is not built.\n{KEPT}\n\n{}",
                switch("Wait for the limit to reset, or add a model to fallback in the config.")
            ),
            ApiErrorKind::Unavailable => format!(
                "⚠️ {api} is overloaded and did not answer, the digest is not built.\n{KEPT}\n\n{}",
                switch("Send /run later to try again, or switch the model with /model.")
            ),
            ApiErrorKind::Auth => format!(
                "⚠️ {api} did not accept the API key, the digest is not built.\n{KEPT}\n\n\
                 Check providers.{}.api_key in config.toml.",
                current.split('/').next().unwrap_or("")
            ),
        }
    }

    /// Read channel titles and usernames again before collecting.
    ///
    /// The list itself is managed only by `/add` and `/rm`, this never changes it.
    /// But a renamed channel would otherwise stay in the digest forever with a stale link.
    pub async fn refresh_channels(&self) -> Result<()> {
        match channels::refresh_names(&self.client, &self.db).await {
            Ok(_) => Ok(()),
            Err(e) if tg::is_fatal(&e) => Err(e),
            // We could not read the names again — this is no reason to cancel the run:
            // the links stay the same, the digest is still built.
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "could not update channel titles");
                Ok(())
            }
        }
    }
}

/// The log span of the work on one theme: its lines start with `run{theme=…}`.
pub fn theme_span(tag: &str) -> tracing::Span {
    tracing::info_span!("run", theme = %tag)
}

/// How much of each old topic goes into the prompt: enough to know the story.
const PREVIOUS_TOPIC_CHARS: usize = 300;
/// From this many posts, an answer with fewer than two topics is taken as broken.
const MIN_POSTS_FOR_CHECK: usize = 30;

/// "Title — start of the text" of one topic, for the next run.
fn short_topic(title: &str, summary: &str) -> String {
    let text: String = summary.chars().take(PREVIOUS_TOPIC_CHARS).collect();
    format!("{title} — {text}")
}

/// The system prompt plus the topics of the last delivered digest, so the model does
/// not send the same stories again. Without old topics the prompts stay as they are.
fn with_previous_topics(prompts: &Prompts, previous: &[String]) -> Prompts {
    if previous.is_empty() {
        return prompts.clone();
    }
    let list: String = previous.iter().map(|t| format!("- {t}\n")).collect();
    let mut out = prompts.clone();
    out.system = format!(
        "{}\n\nThe last digest already had these stories:\n{list}\
         Do NOT repeat them. Leave a story out if the new posts only say the same again. \
         If there is real news about one of them, write only what is new.",
        prompts.system.trim_end()
    );
    out
}
