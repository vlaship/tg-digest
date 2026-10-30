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
#[derive(Debug, Clone, Copy)]
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

    /// Debug commands may sync folders, but must not reset daemon cursors.
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

    /// A channel from the folder. Returns `true` if the cursor was reset: the channel is
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

    /// Channels that are not in the folder are not deleted but disabled: `/list` still
    /// shows them together with the last error.
    pub async fn disable_channels_except(&self, keep: &[i64]) -> Result<Vec<Channel>> {
        let mut disabled = Vec::new();
        for channel in self.enabled_channels().await? {
            if !keep.contains(&channel.id) {
                self.conn
                    .execute(
                        "UPDATE channels SET enabled = 0 WHERE id = ?1",
                        params![channel.id],
                    )
                    .await?;
                disabled.push(channel);
            }
        }
        Ok(disabled)
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
    /// coming from the folder and may reset the cursor: updating names has nothing
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

    /// Disable a channel by hand (`/rm` without a folder).
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

    // ------------------------------------------------------------------- runs

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
        chunks: &[(String, i64)],
    ) -> Result<()> {
        let tx = self.conn.transaction().await?;
        tx.execute(
            "UPDATE runs
             SET status = ?2, posts = ?3, topics = ?4, input_tokens = ?5, output_tokens = ?6,
                 cursors = ?7, dest_label = ?8, dest_peer_id = ?9, dest_access_hash = ?10
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

/// `SELECT *` on `runs` — the column order is set by migration #2.
fn row_to_run(row: &libsql::Row) -> Result<Run> {
    let cursors: Option<String> = row.get(10)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn memory_db() -> Db {
        let conn = libsql::Builder::new_local(":memory:")
            .build()
            .await
            .unwrap()
            .connect()
            .unwrap();
        let db = Db { conn };
        db.migrate().await.unwrap();
        db
    }

    fn dest() -> Destination {
        Destination {
            peer_id: 5,
            access_hash: 6,
        }
    }

    async fn set_cursor(db: &Db, id: i64, value: i32) {
        db.conn
            .execute(
                "UPDATE channels SET last_msg_id = ?2 WHERE id = ?1",
                params![id, value],
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let db = memory_db().await;
        db.migrate().await.unwrap();
        assert!(db.channels().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn preview_folder_changes_do_not_change_persistent_cursors() {
        let db = memory_db().await;
        db.upsert_channel(1, 7, None, "News").await.unwrap();
        set_cursor(&db, 1, 500).await;
        db.disable_channel(1).await.unwrap();
        let copy = db.preview_copy().await.unwrap();
        assert_eq!(copy.channel(1).await.unwrap().unwrap().last_msg_id, 500);
        copy.upsert_channel(1, 7, None, "News").await.unwrap();
        assert_eq!(copy.channel(1).await.unwrap().unwrap().last_msg_id, 0);
        let original = db.channel(1).await.unwrap().unwrap();
        assert!(!original.enabled);
        assert_eq!(original.last_msg_id, 500);
    }

    #[tokio::test]
    async fn empty_summary_preserves_usage_and_applies_only_successful_cursors() {
        let db = memory_db().await;
        db.upsert_channel(1, 7, None, "Good").await.unwrap();
        db.upsert_channel(2, 8, None, "Excluded").await.unwrap();
        let id = db.start_run("manual").await.unwrap();
        db.save_empty_cursors(id, &BTreeMap::from([(1, 42)]), 3, (Some(100), Some(20)))
            .await
            .unwrap();
        assert_eq!(db.channel(1).await.unwrap().unwrap().last_msg_id, 0);
        db.complete_run(id, STATUS_EMPTY).await.unwrap();
        let run = db.run(id).await.unwrap().unwrap();
        assert_eq!(
            (run.posts, run.topics, run.input_tokens, run.output_tokens),
            (Some(3), Some(0), Some(100), Some(20))
        );
        assert_eq!(db.channel(1).await.unwrap().unwrap().last_msg_id, 42);
        assert_eq!(db.channel(2).await.unwrap().unwrap().last_msg_id, 0);
    }

    #[tokio::test]
    async fn corrupt_cursors_are_not_silently_discarded() {
        let db = memory_db().await;
        let id = db.start_run("manual").await.unwrap();
        db.conn
            .execute(
                "UPDATE runs SET cursors = 'broken' WHERE id = ?1",
                params![id],
            )
            .await
            .unwrap();
        assert!(db.complete_run(id, STATUS_OK).await.is_err());
    }

    #[tokio::test]
    async fn re_enabling_a_channel_resets_its_cursor() {
        let db = memory_db().await;
        assert!(db.upsert_channel(1, 7, Some("news"), "News").await.unwrap());
        set_cursor(&db, 1, 500).await;
        db.disable_channels_except(&[]).await.unwrap();

        assert!(db.upsert_channel(1, 7, Some("news"), "News").await.unwrap());
        let channel = db.channel(1).await.unwrap().unwrap();
        assert!(channel.enabled);
        assert_eq!(
            channel.last_msg_id, 0,
            "a returning channel is read from the window, not from a month ago"
        );
    }

    #[tokio::test]
    async fn upsert_of_an_active_channel_keeps_the_cursor() {
        let db = memory_db().await;
        db.upsert_channel(1, 7, Some("news"), "News").await.unwrap();
        set_cursor(&db, 1, 500).await;

        assert!(!db
            .upsert_channel(1, 9, Some("news"), "News 2")
            .await
            .unwrap());
        let channel = db.channel(1).await.unwrap().unwrap();
        assert_eq!(channel.last_msg_id, 500);
        assert_eq!(channel.access_hash, 9);
        assert_eq!(channel.title, "News 2");
    }

    #[tokio::test]
    async fn cursors_apply_only_when_the_run_completes() {
        let db = memory_db().await;
        db.upsert_channel(1, 7, Some("news"), "News").await.unwrap();
        let run = db.start_run("manual").await.unwrap();

        db.save_summary(
            run,
            3,
            2,
            (Some(100), Some(200)),
            &BTreeMap::from([(1i64, 42i32)]),
            "me",
            dest(),
            &[("<b>a</b>".to_string(), 111)],
        )
        .await
        .unwrap();

        assert_eq!(
            db.channel(1).await.unwrap().unwrap().last_msg_id,
            0,
            "before publishing the cursor stays in place"
        );
        assert!(
            db.unfinished_run().await.unwrap().is_some(),
            "the run blocks a new collection"
        );

        db.complete_run(run, STATUS_OK).await.unwrap();
        assert_eq!(db.channel(1).await.unwrap().unwrap().last_msg_id, 42);
        assert!(db.unfinished_run().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn abandon_leaves_cursors_alone() {
        let db = memory_db().await;
        db.upsert_channel(1, 7, Some("news"), "News").await.unwrap();
        let run = db.start_run("cron").await.unwrap();
        db.save_summary(
            run,
            1,
            1,
            (None, None),
            &BTreeMap::from([(1i64, 99i32)]),
            "me",
            dest(),
            &[("x".to_string(), 1)],
        )
        .await
        .unwrap();

        db.abandon_run(run).await.unwrap();
        assert_eq!(db.channel(1).await.unwrap().unwrap().last_msg_id, 0);
        assert!(db.unfinished_run().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cursor_never_moves_backwards() {
        let db = memory_db().await;
        db.upsert_channel(1, 7, Some("news"), "News").await.unwrap();
        set_cursor(&db, 1, 500).await;

        let run = db.start_run("manual").await.unwrap();
        db.save_summary(
            run,
            1,
            1,
            (None, None),
            &BTreeMap::from([(1i64, 10i32)]),
            "me",
            dest(),
            &[("x".to_string(), 1)],
        )
        .await
        .unwrap();
        db.complete_run(run, STATUS_OK).await.unwrap();

        assert_eq!(db.channel(1).await.unwrap().unwrap().last_msg_id, 500);
    }

    #[tokio::test]
    async fn delivery_is_tracked_per_chunk() {
        let db = memory_db().await;
        let run = db.start_run("manual").await.unwrap();
        db.save_summary(
            run,
            1,
            1,
            (None, None),
            &BTreeMap::new(),
            "me",
            dest(),
            &[("a".into(), 11), ("b".into(), 22)],
        )
        .await
        .unwrap();

        db.mark_delivered(run, 0, Some(1234)).await.unwrap();
        let chunks = db.chunks(run).await.unwrap();
        assert!(chunks[0].delivered_at.is_some());
        assert_eq!(chunks[0].sent_msg_id, Some(1234));
        assert!(chunks[1].delivered_at.is_none());
        assert_eq!(chunks[1].random_id, 22);
    }

    #[tokio::test]
    async fn first_send_at_is_written_once() {
        let db = memory_db().await;
        let run = db.start_run("cron").await.unwrap();
        db.mark_first_send(run).await.unwrap();
        let first = db.run(run).await.unwrap().unwrap().first_send_at.unwrap();
        db.mark_first_send(run).await.unwrap();
        assert_eq!(
            db.run(run).await.unwrap().unwrap().first_send_at.unwrap(),
            first
        );
    }

    #[tokio::test]
    async fn renaming_a_channel_touches_neither_cursor_nor_enabled() {
        let db = memory_db().await;
        db.upsert_channel(1, 7, Some("old"), "Old name")
            .await
            .unwrap();
        set_cursor(&db, 1, 500).await;

        // The channel became private: the username is gone, links become /c/…
        db.update_channel_names(1, None, "New name").await.unwrap();

        let channel = db.channel(1).await.unwrap().unwrap();
        assert_eq!(channel.title, "New name");
        assert_eq!(channel.username, None);
        assert_eq!(
            channel.last_msg_id, 500,
            "renaming does not touch the cursor"
        );
        assert!(channel.enabled);
        assert_eq!(channel.post_url(3), "https://t.me/c/1/3");
    }

    #[tokio::test]
    async fn empty_run_counts_as_finished_for_the_scheduler() {
        let db = memory_db().await;
        let run = db.start_run("cron").await.unwrap();
        db.complete_run(run, STATUS_EMPTY).await.unwrap();
        assert_eq!(db.last_finished_run().await.unwrap().unwrap().id, run);
    }

    #[tokio::test]
    async fn only_summarized_and_publishing_runs_block_the_next_collection() {
        let db = memory_db().await;
        let run = db.start_run("cron").await.unwrap();

        // During collection the run saved nothing and blocks nobody.
        assert!(!db.run(run).await.unwrap().unwrap().is_unfinished());

        db.save_summary(
            run,
            1,
            1,
            (None, None),
            &BTreeMap::new(),
            "me",
            dest(),
            &[("x".into(), 1)],
        )
        .await
        .unwrap();
        assert!(db.run(run).await.unwrap().unwrap().is_unfinished());

        db.mark_publishing(run).await.unwrap();
        assert!(db.run(run).await.unwrap().unwrap().is_unfinished());

        db.complete_run(run, STATUS_OK).await.unwrap();
        assert!(!db.run(run).await.unwrap().unwrap().is_unfinished());
    }

    #[tokio::test]
    async fn age_is_counted_from_the_first_send_or_from_the_start() {
        let db = memory_db().await;
        let run_id = db.start_run("cron").await.unwrap();

        // Failed between `summarized` and the first send — the age is counted from the start.
        let run = db.run(run_id).await.unwrap().unwrap();
        assert_eq!(run.age_reference(), run.started_at);

        db.mark_first_send(run_id).await.unwrap();
        let run = db.run(run_id).await.unwrap().unwrap();
        assert_eq!(run.age_reference(), run.first_send_at.unwrap());
    }

    #[tokio::test]
    async fn interrupted_collection_is_closed_and_frees_nothing_else() {
        let db = memory_db().await;
        let collecting = db.start_run("cron").await.unwrap();
        let publishing = db.start_run("cron").await.unwrap();
        db.save_summary(
            publishing,
            1,
            1,
            (None, None),
            &BTreeMap::new(),
            "me",
            dest(),
            &[("x".into(), 1)],
        )
        .await
        .unwrap();

        assert_eq!(db.close_interrupted_runs().await.unwrap(), 1);
        let run = db.run(collecting).await.unwrap().unwrap();
        assert_eq!(run.status, STATUS_ERROR);
        assert_eq!(run.error.as_deref(), Some("interrupted"));
        assert_eq!(db.unfinished_run().await.unwrap().unwrap().id, publishing);
    }

    #[tokio::test]
    async fn channels_are_found_by_username_and_by_id_in_both_forms() {
        let db = memory_db().await;
        db.upsert_channel(1234567890, 7, Some("news"), "News")
            .await
            .unwrap();
        db.upsert_channel(42, 7, None, "Private").await.unwrap();

        assert!(db.find_channel("@news").await.unwrap().is_some());
        assert!(db.find_channel("news").await.unwrap().is_some());
        assert!(db.find_channel("42").await.unwrap().is_some());
        assert!(db.find_channel("-1001234567890").await.unwrap().is_some());
        assert!(db.find_channel("@nope").await.unwrap().is_none());
    }
}
