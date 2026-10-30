//! Database: channels with cursors, journal of runs and journal of digest part delivery.
//!
//! It replaced `state.json`. The reason is not the amount of data, but that the state
//! is no longer one number per channel: an unfinished run must survive a container
//! restart together with the text of its parts and their `random_id`, or after a
//! failure in the middle of publishing there is nothing to recover delivery from.
//!
//! The rule this all exists for: **channel cursors move in one transaction with
//! the run status change to `ok` or `empty`**. Until then they sit in
//! `runs.cursors` and do not affect selection — a failed publish does not "eat"
//! posts that nobody saw.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use libsql::{params, Connection};
use std::collections::BTreeMap;

/// The run is going: posts are being collected, there is no model answer yet.
pub const STATUS_COLLECTING: &str = "collecting";
/// The model answer is received, the parts are in the journal, the destination is fixed.
pub const STATUS_SUMMARIZED: &str = "summarized";
/// The parts are being sent.
pub const STATUS_PUBLISHING: &str = "publishing";
/// The digest is fully delivered, the cursors are applied.
pub const STATUS_OK: &str = "ok";
/// There was nothing to post; the posts are read, the cursors are applied.
pub const STATUS_EMPTY: &str = "empty";
/// The run did not happen; the cursors are not applied.
pub const STATUS_ERROR: &str = "error";

/// The full schema: migrations are applied in order, the version is stored in
/// `schema_version`. Add only to the end of the list.
const MIGRATIONS: &[&str] = &[
    // 1
    "CREATE TABLE channels (
        id          INTEGER PRIMARY KEY,
        access_hash INTEGER NOT NULL,
        username    TEXT,
        title       TEXT NOT NULL,
        enabled     INTEGER NOT NULL DEFAULT 1,
        last_msg_id INTEGER NOT NULL DEFAULT 0,
        last_error  TEXT,
        added_at    TEXT NOT NULL
    )",
    // 2
    "CREATE TABLE runs (
        id                INTEGER PRIMARY KEY AUTOINCREMENT,
        started_at        TEXT NOT NULL,
        finished_at       TEXT,
        status            TEXT NOT NULL,
        trigger           TEXT NOT NULL,
        posts             INTEGER,
        topics            INTEGER,
        input_tokens      INTEGER,
        output_tokens     INTEGER,
        error             TEXT,
        cursors           TEXT,
        first_send_at     TEXT,
        dest_label        TEXT,
        dest_peer_id      INTEGER,
        dest_access_hash  INTEGER,
        stale_notified_at TEXT
    )",
    // 3
    "CREATE TABLE run_chunks (
        run_id       INTEGER NOT NULL REFERENCES runs(id),
        idx          INTEGER NOT NULL,
        html         TEXT NOT NULL,
        random_id    INTEGER NOT NULL,
        delivered_at TEXT,
        sent_msg_id  INTEGER,
        PRIMARY KEY (run_id, idx)
    )",
    // 4: other recipients of the digest (JSON list of `Destination`), fixed at
    // `summarized` time like the main destination.
    "ALTER TABLE runs ADD COLUMN extra_dests TEXT",
    // 5: users who turned the digest off with /stop.
    "CREATE TABLE muted_users (user_id INTEGER PRIMARY KEY)",
    // 6: short topics of the digest (JSON list of strings), so the next run can skip
    // stories that were already sent.
    "ALTER TABLE runs ADD COLUMN digest_topics TEXT",
];

/// A channel in the database. `id` is the "bare" Telegram channel id (positive,
/// without the `-100` prefix), as MTProto gives it.
#[derive(Debug, Clone)]
pub struct Channel {
    pub id: i64,
    pub access_hash: i64,
    pub username: Option<String>,
    pub title: String,
    pub enabled: bool,
    pub last_msg_id: i32,
    pub last_error: Option<String>,
}

impl Channel {
    /// Link to a post: for public channels by username, for private ones through `/c/`.
    pub fn post_url(&self, msg_id: i32) -> String {
        match &self.username {
            Some(name) => format!("https://t.me/{name}/{msg_id}"),
            None => format!("https://t.me/c/{}/{}", self.id, msg_id),
        }
    }

    /// How the channel is named in commands and logs.
    pub fn label(&self) -> String {
        match &self.username {
            Some(name) => format!("@{name}"),
            None => format!("id {}", self.id),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Run {
    pub id: i64,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: String,
    pub trigger: String,
    pub posts: Option<i64>,
    pub topics: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub error: Option<String>,
    pub cursors: BTreeMap<i64, i32>,
    pub first_send_at: Option<DateTime<Utc>>,
    pub dest_label: Option<String>,
    pub dest_peer_id: Option<i64>,
    pub dest_access_hash: Option<i64>,
    pub stale_notified_at: Option<DateTime<Utc>>,
    /// Other users who also get the digest (`bot.allowed_users` who started the bot).
    pub extra_dests: Vec<Destination>,
}

impl Run {
    /// An unfinished run blocks a new collection: the cursors have not moved, a second
    /// collection would give the same posts and a second digest.
    pub fn is_unfinished(&self) -> bool {
        self.status == STATUS_SUMMARIZED || self.status == STATUS_PUBLISHING
    }

    /// What the age is counted from: from the first send attempt, or if it did not
    /// get that far — from the run start.
    pub fn age_reference(&self) -> DateTime<Utc> {
        self.first_send_at.unwrap_or(self.started_at)
    }

    /// The destination resolved at `summarized` time. Recovery sends here,
    /// not to what is in the config now.
    pub fn destination(&self) -> Option<Destination> {
        Some(Destination {
            peer_id: self.dest_peer_id?,
            access_hash: self.dest_access_hash?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Chunk {
    pub idx: i64,
    pub html: String,
    pub random_id: i64,
    pub delivered_at: Option<DateTime<Utc>>,
    pub sent_msg_id: Option<i64>,
}

/// Where the digest went (or will go). Resolved once, when the parts are written.
/// `peer_id` is a Bot API dialog id, so the peer type is restored without doubt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Destination {
    pub peer_id: i64,
    pub access_hash: i64,
}

/// One connection per process. Only the run and the commands write to the database,
/// and they work under one lock (`run::Runner`), so two transactions never
/// overlap; read-only commands (`/list`, `/status`) go without the lock,
/// and that is enough for them.
pub struct Db {
    conn: Connection,
}

impl Db {
    pub async fn open(path: &str) -> Result<Self> {
        let conn = libsql::Builder::new_local(path)
            .build()
            .await
            .with_context(|| format!("could not open the database {path}"))?
            .connect()
            .with_context(|| format!("could not connect to the database {path}"))?;

        let db = Self { conn };
        db.migrate().await.context("database migration failed")?;
        Ok(db)
    }

    async fn migrate(&self) -> Result<()> {
        self.conn
            .execute(
                "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)",
                params![],
            )
            .await?;

        let current: i64 = match self
            .conn
            .prepare("SELECT version FROM schema_version LIMIT 1")
            .await?
            .query_row(params![])
            .await
        {
            Ok(row) => row.get(0)?,
            Err(libsql::Error::QueryReturnedNoRows) => {
                self.conn
                    .execute("INSERT INTO schema_version (version) VALUES (0)", params![])
                    .await?;
                0
            }
            Err(e) => return Err(e.into()),
        };

        if current as usize >= MIGRATIONS.len() {
            return Ok(());
        }

        let tx = self.conn.transaction().await?;
        for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
            tx.execute(sql, params![])
                .await
                .with_context(|| format!("migration #{} was not applied", index + 1))?;
        }
        tx.execute(
            "UPDATE schema_version SET version = ?1",
            params![MIGRATIONS.len() as i64],
        )
        .await?;
        tx.commit().await?;

        tracing::info!(
            from = current,
            to = MIGRATIONS.len(),
            "database schema updated"
        );
        Ok(())
    }

    // --------------------------------------------------------------- channels

    /// Debug commands (`preview`, `run --dry-run`) work on a copy: refreshing names there
    /// must not change the daemon database.
    pub async fn preview_copy(&self) -> Result<Self> {
        let copy = Self::open(":memory:").await?;
        for channel in self.channels().await? {
            copy.conn.execute(
                "INSERT INTO channels (id, access_hash, username, title, enabled, last_msg_id, last_error, added_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![channel.id, channel.access_hash, channel.username, channel.title,
                    channel.enabled as i64, channel.last_msg_id, channel.last_error, Utc::now().to_rfc3339()],
            ).await?;
        }
        Ok(copy)
    }

    pub async fn channels(&self) -> Result<Vec<Channel>> {
        let rows = self
            .conn
            .prepare(
                "SELECT id, access_hash, username, title, enabled, last_msg_id, last_error
                 FROM channels ORDER BY title",
            )
            .await?
            .query(params![])
            .await?;
        collect_channels(rows).await
    }

    pub async fn enabled_channels(&self) -> Result<Vec<Channel>> {
        let rows = self
            .conn
            .prepare(
                "SELECT id, access_hash, username, title, enabled, last_msg_id, last_error
                 FROM channels WHERE enabled = 1 ORDER BY title",
            )
            .await?
            .query(params![])
            .await?;
        collect_channels(rows).await
    }

    pub async fn channel(&self, id: i64) -> Result<Option<Channel>> {
        let rows = self
            .conn
            .prepare(
                "SELECT id, access_hash, username, title, enabled, last_msg_id, last_error
                 FROM channels WHERE id = ?1",
            )
            .await?
            .query(params![id])
            .await?;
        Ok(collect_channels(rows).await?.pop())
    }

    /// A channel from `/add`. Returns `true` if the cursor was reset: the channel is
    /// new or came back from the disabled ones (otherwise a channel disabled for a month
    /// would bring a month of posts when it comes back).
    pub async fn upsert_channel(
        &self,
        id: i64,
        access_hash: i64,
        username: Option<&str>,
        title: &str,
    ) -> Result<bool> {
        let existing = self.channel(id).await?;

        match existing {
            None => {
                self.conn
                    .execute(
                        "INSERT INTO channels
                           (id, access_hash, username, title, enabled, last_msg_id, added_at)
                         VALUES (?1, ?2, ?3, ?4, 1, 0, ?5)",
                        params![id, access_hash, username, title, Utc::now().to_rfc3339()],
                    )
                    .await?;
                Ok(true)
            }
            Some(channel) if !channel.enabled => {
                self.conn
                    .execute(
                        "UPDATE channels
                         SET access_hash = ?2, username = ?3, title = ?4,
                             enabled = 1, last_msg_id = 0, last_error = NULL
                         WHERE id = ?1",
                        params![id, access_hash, username, title],
                    )
                    .await?;
                Ok(true)
            }
            Some(_) => {
                self.conn
                    .execute(
                        "UPDATE channels SET access_hash = ?2, username = ?3, title = ?4
                         WHERE id = ?1",
                        params![id, access_hash, username, title],
                    )
                    .await?;
                Ok(false)
            }
        }
    }

    pub async fn set_channel_error(&self, id: i64, error: Option<&str>) -> Result<()> {
        self.conn
            .execute(
                "UPDATE channels SET last_error = ?2 WHERE id = ?1",
                params![id, error],
            )
            .await?;
        Ok(())
    }

    /// Update only the title and the username.
    ///
    /// Separate from `upsert_channel`, because that one treats the channel as
    /// coming from `/add` and may reset the cursor: updating names has nothing
    /// to do with the channel list.
    pub async fn update_channel_names(
        &self,
        id: i64,
        username: Option<&str>,
        title: &str,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE channels SET username = ?2, title = ?3 WHERE id = ?1",
                params![id, username, title],
            )
            .await?;
        Ok(())
    }

    /// `/rm`: disable a channel. It is not deleted: `/list` still shows it.
    pub async fn disable_channel(&self, id: i64) -> Result<bool> {
        let affected = self
            .conn
            .execute("UPDATE channels SET enabled = 0 WHERE id = ?1", params![id])
            .await?;
        Ok(affected > 0)
    }

    /// `/reset`: read the channel again from the `lookback_hours` window.
    pub async fn reset_channel(&self, id: i64) -> Result<bool> {
        let affected = self
            .conn
            .execute(
                "UPDATE channels SET last_msg_id = 0, last_error = NULL WHERE id = ?1",
                params![id],
            )
            .await?;
        Ok(affected > 0)
    }

    /// A channel by `@username` or by numeric id — private channels have no username.
    /// We also accept the `-100…` form: Telegram clients show it.
    pub async fn find_channel(&self, key: &str) -> Result<Option<Channel>> {
        let key = key.trim();
        if let Ok(id) = key.parse::<i64>() {
            if let Some(channel) = self.channel(bare_channel_id(id)).await? {
                return Ok(Some(channel));
            }
            return Ok(None);
        }
        let name = key.trim_start_matches('@').to_lowercase();
        Ok(self.channels().await?.into_iter().find(|c| {
            c.username.as_deref().map(str::to_lowercase).as_deref() == Some(name.as_str())
        }))
    }

    // ------------------------------------------------------------------ users

    /// `/stop` (`muted = true`) and `/start` (`muted = false`) for one user.
    pub async fn set_muted(&self, user_id: i64, muted: bool) -> Result<()> {
        let sql = if muted {
            "INSERT OR IGNORE INTO muted_users (user_id) VALUES (?1)"
        } else {
            "DELETE FROM muted_users WHERE user_id = ?1"
        };
        self.conn.execute(sql, params![user_id]).await?;
        Ok(())
    }

    /// Users who do not want the digest.
    pub async fn muted_users(&self) -> Result<Vec<i64>> {
        let mut rows = self
            .conn
            .query("SELECT user_id FROM muted_users", ())
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row.get(0)?);
        }
        Ok(out)
    }

    // ------------------------------------------------------------------- runs

    /// Short topics of a digest, saved with the run (see `last_digest_topics`).
    pub async fn set_digest_topics(&self, run_id: i64, topics: &[String]) -> Result<()> {
        self.conn
            .execute(
                "UPDATE runs SET digest_topics = ?2 WHERE id = ?1",
                params![run_id, serde_json::to_string(topics)?],
            )
            .await?;
        Ok(())
    }

    /// Topics of the last delivered digest: the next run must not repeat them.
    pub async fn last_digest_topics(&self) -> Result<Vec<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT digest_topics FROM runs
                 WHERE status = 'ok' AND digest_topics IS NOT NULL
                 ORDER BY id DESC LIMIT 1",
                (),
            )
            .await?;
        Ok(match rows.next().await? {
            Some(row) => serde_json::from_str(&row.get::<String>(0)?)
                .context("digest topics in the database are damaged")?,
            None => Vec::new(),
        })
    }

    pub async fn start_run(&self, trigger: &str) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO runs (started_at, status, trigger) VALUES (?1, ?2, ?3)",
                params![Utc::now().to_rfc3339(), STATUS_COLLECTING, trigger],
            )
            .await?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The run that blocks a new collection (`summarized` or `publishing`).
    pub async fn unfinished_run(&self) -> Result<Option<Run>> {
        self.query_run(
            "SELECT * FROM runs WHERE status IN ('summarized','publishing') ORDER BY id LIMIT 1",
            params![],
        )
        .await
    }

    pub async fn run(&self, id: i64) -> Result<Option<Run>> {
        self.query_run("SELECT * FROM runs WHERE id = ?1", params![id])
            .await
    }

    pub async fn last_run(&self) -> Result<Option<Run>> {
        self.query_run("SELECT * FROM runs ORDER BY id DESC LIMIT 1", params![])
            .await
    }

    /// The last run that reached the end: the scheduler counts catch-up from it.
    /// An empty run is also successful, or after a quiet day every restart
    /// would run the catch-up again.
    pub async fn last_finished_run(&self) -> Result<Option<Run>> {
        self.query_run(
            "SELECT * FROM runs WHERE status IN ('ok','empty') ORDER BY started_at DESC LIMIT 1",
            params![],
        )
        .await
    }

    /// The last published digest — for `/last`.
    pub async fn last_ok_run(&self) -> Result<Option<Run>> {
        self.query_run(
            "SELECT * FROM runs WHERE status = 'ok' ORDER BY id DESC LIMIT 1",
            params![],
        )
        .await
    }

    async fn query_run<P: libsql::params::IntoParams>(
        &self,
        sql: &str,
        params: P,
    ) -> Result<Option<Run>> {
        let mut rows = self.conn.prepare(sql).await?.query(params).await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        Ok(Some(row_to_run(&row)?))
    }

    /// A run stopped before the model answer: nothing is saved, the cursors are safe.
    pub async fn fail_run(&self, id: i64, error: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE runs SET status = ?2, error = ?3, finished_at = ?4 WHERE id = ?1",
                params![id, STATUS_ERROR, error, Utc::now().to_rfc3339()],
            )
            .await?;
        Ok(())
    }

    /// The model answer is received. In one transaction: status, metrics, cursors,
    /// the resolved destination and all parts with `random_id`. From this moment
    /// neither the digest nor the destination is calculated again.
    #[allow(clippy::too_many_arguments)]
    pub async fn save_summary(
        &self,
        run_id: i64,
        posts: usize,
        topics: usize,
        usage: (Option<u64>, Option<u64>),
        cursors: &BTreeMap<i64, i32>,
        dest_label: &str,
        dest: Destination,
        extra_dests: &[Destination],
        chunks: &[(String, i64)],
    ) -> Result<()> {
        let tx = self.conn.transaction().await?;
        tx.execute(
            "UPDATE runs
             SET status = ?2, posts = ?3, topics = ?4, input_tokens = ?5, output_tokens = ?6,
                 cursors = ?7, dest_label = ?8, dest_peer_id = ?9, dest_access_hash = ?10,
                 extra_dests = ?11
             WHERE id = ?1",
            params![
                run_id,
                STATUS_SUMMARIZED,
                posts as i64,
                topics as i64,
                usage.0.map(|v| v as i64),
                usage.1.map(|v| v as i64),
                serde_json::to_string(cursors)?,
                dest_label,
                dest.peer_id,
                dest.access_hash,
                serde_json::to_string(extra_dests)?,
            ],
        )
        .await?;

        for (idx, (html, random_id)) in chunks.iter().enumerate() {
            tx.execute(
                "INSERT INTO run_chunks (run_id, idx, html, random_id) VALUES (?1, ?2, ?3, ?4)",
                params![run_id, idx as i64, html.as_str(), *random_id],
            )
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Cursors of an empty run: there is nothing to post, but the posts are read.
    pub async fn save_empty_cursors(
        &self,
        run_id: i64,
        cursors: &BTreeMap<i64, i32>,
        posts: usize,
        usage: (Option<u64>, Option<u64>),
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE runs SET cursors = ?2, posts = ?3, topics = 0,
                 input_tokens = ?4, output_tokens = ?5 WHERE id = ?1",
                params![
                    run_id,
                    serde_json::to_string(cursors)?,
                    posts as i64,
                    usage.0.map(|v| v as i64),
                    usage.1.map(|v| v as i64)
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn mark_publishing(&self, run_id: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE runs SET status = ?2 WHERE id = ?1",
                params![run_id, STATUS_PUBLISHING],
            )
            .await?;
        Ok(())
    }

    /// Time of the first send attempt: the age is counted from it, so it is
    /// written exactly once.
    pub async fn mark_first_send(&self, run_id: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE runs SET first_send_at = ?2 WHERE id = ?1 AND first_send_at IS NULL",
                params![run_id, Utc::now().to_rfc3339()],
            )
            .await?;
        Ok(())
    }

    pub async fn mark_stale_notified(&self, run_id: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE runs SET stale_notified_at = ?2 WHERE id = ?1",
                params![run_id, Utc::now().to_rfc3339()],
            )
            .await?;
        Ok(())
    }

    pub async fn chunks(&self, run_id: i64) -> Result<Vec<Chunk>> {
        let mut rows = self
            .conn
            .prepare(
                "SELECT idx, html, random_id, delivered_at, sent_msg_id
                 FROM run_chunks WHERE run_id = ?1 ORDER BY idx",
            )
            .await?
            .query(params![run_id])
            .await?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(Chunk {
                idx: row.get(0)?,
                html: row.get(1)?,
                random_id: row.get(2)?,
                delivered_at: parse_time(row.get::<Option<String>>(3)?)?,
                sent_msg_id: row.get(4)?,
            });
        }
        Ok(out)
    }

    /// The completion rule is `delivered_at`, not `sent_msg_id`:
    /// for some peers Telegram does not return the id of the sent message.
    pub async fn mark_delivered(
        &self,
        run_id: i64,
        idx: i64,
        sent_msg_id: Option<i32>,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE run_chunks SET delivered_at = ?3, sent_msg_id = ?4
                 WHERE run_id = ?1 AND idx = ?2",
                params![
                    run_id,
                    idx,
                    Utc::now().to_rfc3339(),
                    sent_msg_id.map(|v| v as i64)
                ],
            )
            .await?;
        Ok(())
    }

    /// End of the run: status and cursors in one transaction.
    pub async fn complete_run(&self, run_id: i64, status: &str) -> Result<()> {
        let run = self
            .run(run_id)
            .await?
            .with_context(|| format!("run #{run_id} disappeared from the database"))?;

        let tx = self.conn.transaction().await?;
        tx.execute(
            "UPDATE runs SET status = ?2, finished_at = ?3, error = NULL WHERE id = ?1",
            params![run_id, status, Utc::now().to_rfc3339()],
        )
        .await?;

        // We move the cursor only forward: a manual run for an old period must not
        // move the channel back and bring duplicates into the next digest.
        for (channel_id, msg_id) in &run.cursors {
            tx.execute(
                "UPDATE channels SET last_msg_id = MAX(last_msg_id, ?2) WHERE id = ?1",
                params![*channel_id, *msg_id],
            )
            .await?;
        }
        tx.commit().await?;

        tracing::info!(run = run_id, %status, channels = run.cursors.len(), "cursors applied");
        Ok(())
    }

    /// `/abandon`: release the lock, do **not** apply the cursors — the delivered
    /// parts stay in the chat, the posts go into the next digest.
    pub async fn abandon_run(&self, run_id: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE runs SET status = ?2, error = 'abandoned', finished_at = ?3 WHERE id = ?1",
                params![run_id, STATUS_ERROR, Utc::now().to_rfc3339()],
            )
            .await?;
        Ok(())
    }

    /// Runs that stopped during collection (failed before the model answer): nothing is
    /// saved, they block nothing, but they must not stay in `collecting`.
    pub async fn close_interrupted_runs(&self) -> Result<usize> {
        let affected = self
            .conn
            .execute(
                "UPDATE runs SET status = ?1, error = 'interrupted', finished_at = ?2
                 WHERE status = ?3",
                params![STATUS_ERROR, Utc::now().to_rfc3339(), STATUS_COLLECTING],
            )
            .await?;
        Ok(affected as usize)
    }
}

/// `-1001234567890` from a Telegram client and `1234567890` from MTProto are the
/// same channel.
pub fn bare_channel_id(id: i64) -> i64 {
    if id <= -1_000_000_000_000 {
        -id - 1_000_000_000_000
    } else {
        id.abs()
    }
}

async fn collect_channels(mut rows: libsql::Rows) -> Result<Vec<Channel>> {
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        out.push(Channel {
            id: row.get(0)?,
            access_hash: row.get(1)?,
            username: row.get(2)?,
            title: row.get(3)?,
            enabled: row.get::<i64>(4)? != 0,
            last_msg_id: row.get(5)?,
            last_error: row.get(6)?,
        });
    }
    Ok(out)
}

/// `SELECT *` on `runs` — the column order is set by migration #2, plus
/// `extra_dests` from migration #4 at the end.
fn row_to_run(row: &libsql::Row) -> Result<Run> {
    let cursors: Option<String> = row.get(10)?;
    let extra_dests: Option<String> = row.get(16)?;
    Ok(Run {
        id: row.get(0)?,
        started_at: parse_time(row.get::<Option<String>>(1)?)?.unwrap_or_else(Utc::now),
        finished_at: parse_time(row.get::<Option<String>>(2)?)?,
        status: row.get(3)?,
        trigger: row.get(4)?,
        posts: row.get(5)?,
        topics: row.get(6)?,
        input_tokens: row.get(7)?,
        output_tokens: row.get(8)?,
        error: row.get(9)?,
        cursors: match cursors {
            Some(raw) => {
                serde_json::from_str(&raw).context("run cursors in the database are damaged")?
            }
            None => BTreeMap::new(),
        },
        first_send_at: parse_time(row.get::<Option<String>>(11)?)?,
        dest_label: row.get(12)?,
        dest_peer_id: row.get(13)?,
        dest_access_hash: row.get(14)?,
        stale_notified_at: parse_time(row.get::<Option<String>>(15)?)?,
        extra_dests: match extra_dests {
            Some(raw) => {
                serde_json::from_str(&raw).context("run recipients in the database are damaged")?
            }
            None => Vec::new(),
        },
    })
}

fn parse_time(raw: Option<String>) -> Result<Option<DateTime<Utc>>> {
    match raw {
        None => Ok(None),
        Some(s) => Ok(Some(
            DateTime::parse_from_rfc3339(&s)
                .with_context(|| format!("could not parse the time {s:?} from the database"))?
                .with_timezone(&Utc),
        )),
    }
}
