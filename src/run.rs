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

use crate::cancel::Cancel;
use crate::config::{Config, Prompts};
use crate::db::{self, Db, NewChunk, Run};
use crate::digest::{self, RenderOptions};
use crate::summarize::Summarizer;
use crate::telegram::bot::Bot;
use crate::telegram::{channels, client as tg, photos, publish};

pub const TRIGGER_CRON: &str = "cron";
pub const TRIGGER_CATCH_UP: &str = "catch_up";
pub const TRIGGER_MANUAL: &str = "manual";

/// Key in the `settings` table for the provider the owner chose.
const PROVIDER_SETTING: &str = "provider";

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
            Outcome::Failed(id, reason) => write!(f, "run #{id} failed: {reason}"),
            Outcome::Abandoned(id) => write!(
                f,
                "run #{id} cancelled, cursors not moved — the posts go into the next digest"
            ),
        }
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
    async fn notify(&self, text: &str) {
        match &self.bot {
            Some(bot) => bot.notify(text).await,
            None => tracing::warn!(%text, "notification (no bot in this command)"),
        }
    }

    /// The model provider for the next run: the one the owner chose with `/provider`
    /// or `/fallback` (kept in the database), else `provider` from the config.
    pub async fn provider(&self) -> String {
        match self.db.setting(PROVIDER_SETTING).await {
            Ok(Some(name)) if self.cfg.check_provider(&name).is_ok() => name,
            Ok(_) => self.cfg.provider.clone(),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "could not read the provider setting");
                self.cfg.provider.clone()
            }
        }
    }

    /// Switch the model provider. The name and its key are checked first.
    pub async fn set_provider(&self, name: &str) -> Result<()> {
        self.cfg.check_provider(name)?;
        self.db.set_setting(PROVIDER_SETTING, name).await?;
        tracing::info!(provider = name, "model provider switched");
        Ok(())
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

    /// Entry point of every trigger: cron, catch-up, `/run`, daemon start.
    pub async fn trigger(&self, trigger: &str) -> Result<Outcome> {
        let Some(_guard) = self.try_lock() else {
            let busy = self.db.last_run().await?.map(|r| r.id).unwrap_or_default();
            return Ok(Outcome::Busy(busy));
        };

        // The check for an unfinished run happens on EVERY trigger, not only
        // on start: otherwise a second digest would go on top of the first one.
        if let Some(run) = self.db.unfinished_run().await? {
            return self.finish_unfinished(run).await;
        }

        self.fresh_run(trigger).await
    }

    /// `/resume`: the owner accepts the risk of a duplicate. Delivery uses the saved
    /// `random_id` — the best chance of deduplication if the time has not run out yet.
    pub async fn resume(&self, run_id: i64) -> Result<Outcome> {
        let Some(_guard) = self.try_lock() else {
            return Ok(Outcome::Busy(self.running_id().await?));
        };
        let Some(run) = self.db.run(run_id).await? else {
            return Ok(Outcome::Failed(run_id, "no such run".into()));
        };
        if !run.is_unfinished() {
            return Ok(Outcome::Failed(
                run_id,
                format!("run has status {}, nothing to deliver", run.status),
            ));
        }
        self.deliver(&run, true).await
    }

    pub async fn abandon(&self, run_id: i64) -> Result<Outcome> {
        let Some(_guard) = self.try_lock() else {
            return Ok(Outcome::Busy(self.running_id().await?));
        };
        let Some(run) = self.db.run(run_id).await? else {
            return Ok(Outcome::Failed(run_id, "no such run".into()));
        };
        if !run.is_unfinished() {
            return Ok(Outcome::Failed(
                run_id,
                format!("run has status {}, nothing to cancel", run.status),
            ));
        }
        self.db.abandon_run(run_id).await?;
        photos::remove(&self.photos_dir(), run_id);
        Ok(Outcome::Abandoned(run_id))
    }

    /// Number of the run that holds the lock now: the last one in the journal.
    async fn running_id(&self) -> Result<i64> {
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
                "run #{} not delivered since {}, a retry is not guaranteed to be safe:\n\
                 /resume {} — deliver the undelivered parts (a duplicate is possible),\n\
                 /abandon {} — cancel",
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
                    self.notify(&format!("digest not fully delivered: {reason}"))
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

        let collected =
            match tg::collect_posts(&self.client, &self.cfg, &channels, &self.cancel).await {
                Ok(collected) => collected,
                Err(e) => return self.fail(run_id, e).await,
            };

        // An excluded channel is a reason for a notification: we cannot silently lose
        // its posts, and its cursor does not move, so the next run reads the rest.
        let failed: Vec<i64> = collected.failures.iter().map(|(c, _)| c.id).collect();
        for (channel, reason) in &collected.failures {
            self.db.set_channel_error(channel.id, Some(reason)).await?;
            self.notify(&format!(
                "channel {} excluded from the run: {reason}",
                channel.label()
            ))
            .await;
        }
        for channel in channels.iter().filter(|c| !failed.contains(&c.id)) {
            if channel.last_error.is_some() {
                self.db.set_channel_error(channel.id, None).await?;
            }
        }

        if collected.posts.is_empty() {
            tracing::info!(run = run_id, "no new posts");
            return self.finish_empty(run_id, &collected, (None, None)).await;
        }

        let provider = self.provider().await;
        let summarizer = match Summarizer::new(&self.cfg, &provider) {
            Ok(s) => s,
            Err(e) => return self.fail(run_id, e).await,
        };
        // A channel often writes about yesterday's story again in a new post: tell the
        // model what the last digest already had.
        let previous = self.db.last_digest_topics().await.unwrap_or_else(|e| {
            tracing::warn!(error = %format!("{e:#}"), "could not read the last digest topics");
            Vec::new()
        });
        let prompts = with_previous_topics(&self.cfg.prompts, &previous);
        let summary = match summarizer
            .summarize(
                &prompts,
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

        let chunks = digest::render_parts(
            &digest,
            &RenderOptions {
                digest_title: &self.cfg.prompts.digest_title,
                tldr_title: &self.cfg.prompts.tldr_title,
                timezone: self.timezone(),
                now: Utc::now(),
            },
        );

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

        let dest_label = self.cfg.telegram.destination.clone();
        // Who gets the digest is fixed now: the first one is the main destination,
        // the rest get copies.
        let resolved = match self.bot() {
            Ok(bot) => publish::recipients(bot, &self.db, &dest_label).await,
            Err(e) => Err(e),
        };
        let mut recipients = match resolved {
            Ok(recipients) => recipients,
            Err(e) => return self.fail(run_id, e).await,
        };
        // Everybody sent /stop: nothing to send, but the posts were read.
        if recipients.is_empty() {
            tracing::info!(
                run = run_id,
                "everybody turned the digest off, nothing is sent"
            );
            return self
                .finish_empty(
                    run_id,
                    &collected,
                    (summary.input_tokens, summary.output_tokens),
                )
                .await;
        }
        let dest = recipients.remove(0);
        let extra_dests = recipients;

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
                &extra_dests,
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
    async fn fail(&self, run_id: i64, error: anyhow::Error) -> Result<Outcome> {
        if tg::is_fatal(&error) {
            // There is no point in waiting out a revoked session — let it fail upwards.
            self.db.fail_run(run_id, &format!("{error:#}")).await?;
            return Err(error);
        }

        let reason = if self.cancel.is_cancelled() {
            "cancelled".to_string()
        } else {
            format!("{error:#}")
        };

        tracing::error!(run = run_id, %reason, "run finished with an error");
        self.db.fail_run(run_id, &reason).await?;

        if !self.cancel.is_cancelled() {
            let limit = error
                .chain()
                .find_map(|e| e.downcast_ref::<crate::summarize::LimitReached>());
            let text = match limit {
                Some(limit) => {
                    let current = self.provider().await;
                    let choice = match &self.cfg.fallback_provider {
                        Some(fallback) if *fallback != current => format!(
                            "Switch to {fallback} and build the digest now? Send /fallback.\n\
                             Later, /provider {current} switches back."
                        ),
                        _ => "Wait for the limit to reset, or set fallback_provider \
                              in the config."
                            .to_string(),
                    };
                    format!(
                        "⚠️ the {} API limit is used up, the digest is not built.\n\
                         The posts are kept and go into the next run.\n\n{choice}\n\n{}",
                        limit.api, limit.details
                    )
                }
                None => format!("digest not built: {reason}"),
            };
            self.notify(&text).await;
        }
        Ok(Outcome::Failed(run_id, reason))
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

/// How much of each old topic goes into the prompt: enough to know the story.
const PREVIOUS_TOPIC_CHARS: usize = 300;

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
