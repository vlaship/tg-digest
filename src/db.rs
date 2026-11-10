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

/// The columns `collect_channels` reads, in its order.
const CHANNEL_COLUMNS: &str =
    "id, access_hash, username, title, enabled, last_msg_id, last_error, theme";

/// The full schema. It is created on the first start. The only migration is the
/// `theme` column (`migrate_themes`); after any other schema change start with a
/// new database.
///
/// A channel row belongs to one theme. The same channel can be in several themes:
/// each row has its own cursor, so the themes do not see each other.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS channels (
    id          INTEGER NOT NULL,
    access_hash INTEGER NOT NULL,
    username    TEXT,
    title       TEXT NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    last_msg_id INTEGER NOT NULL DEFAULT 0,
    last_error  TEXT,
    added_at    TEXT NOT NULL,
    theme       TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (theme, id)
);
CREATE TABLE IF NOT EXISTS runs (
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
    stale_notified_at TEXT,
    -- short topics of the digest (JSON list of strings), so the next run can skip
    -- stories that were already sent
    digest_topics     TEXT,
    -- the theme of the run; new columns go last, `row_to_run` reads by position
    theme             TEXT NOT NULL DEFAULT ''
);
-- photos: how many photos a part has. The files are in the photos folder
-- (`photos.rs`), downloaded once at `summarized` time.
CREATE TABLE IF NOT EXISTS run_chunks (
    run_id       INTEGER NOT NULL REFERENCES runs(id),
    idx          INTEGER NOT NULL,
    html         TEXT NOT NULL,
    random_id    INTEGER NOT NULL,
    delivered_at TEXT,
    sent_msg_id  INTEGER,
    photos       INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (run_id, idx)
);
-- settings the owner changes with commands, like the active model provider
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

/// A channel in the database. `id` is the "bare" Telegram channel id (positive,
/// without the `-100` prefix), as MTProto gives it.
#[derive(Debug, Clone)]
pub struct Channel {
    /// The tag of the theme this row belongs to.
    pub theme: String,
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
    /// The tag of the theme this run built a digest for.
    pub theme: String,
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
    /// Text of the part. With photos it is their caption, it may be empty.
    pub html: String,
    pub random_id: i64,
    pub delivered_at: Option<DateTime<Utc>>,
    pub sent_msg_id: Option<i64>,
    /// How many photos go with the part (files in `photos.rs`), 0 for a text part.
    pub photos: usize,
}

/// A part to write into the journal.
pub struct NewChunk {
    pub html: String,
    pub random_id: i64,
    pub photos: usize,
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
        db.prepare_themes().await?;
        db.conn
            .execute_batch(SCHEMA)
            .await
            .context("could not create the database schema")?;
        db.copy_old_channels().await?;
        Ok(db)
    }

    /// A database from before themes, step one (before the schema): the old
    /// `channels` table is put aside, because the theme is now a part of its key,
    /// and `runs` gets the `theme` column. The old rows have an empty theme until
    /// `adopt_theme` (called by `adopt_old_channels` in `main.rs`) gives them one.
    async fn prepare_themes(&self) -> Result<()> {
        if self.has_table("channels").await? && !self.has_column("channels", "theme").await? {
            tracing::info!("database from before themes: moving the channels");
            self.conn
                .execute("ALTER TABLE channels RENAME TO channels_old", ())
                .await?;
        }
        if self.has_table("runs").await? && !self.has_column("runs", "theme").await? {
            self.conn
                .execute(
                    "ALTER TABLE runs ADD COLUMN theme TEXT NOT NULL DEFAULT ''",
                    (),
                )
                .await?;
        }
        Ok(())
    }

    /// Step two (after the schema): the rows go into the new `channels` table.
    async fn copy_old_channels(&self) -> Result<()> {
        if !self.has_table("channels_old").await? {
            return Ok(());
        }
        let tx = self.conn.transaction().await?;
        tx.execute(
            "INSERT OR IGNORE INTO channels
               (id, access_hash, username, title, enabled, last_msg_id, last_error, added_at)
             SELECT id, access_hash, username, title, enabled, last_msg_id, last_error, added_at
             FROM channels_old",
            (),
        )
        .await?;
        tx.execute("DROP TABLE channels_old", ()).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn has_table(&self, table: &str) -> Result<bool> {
        self.any_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
        )
        .await
    }

    async fn has_column(&self, table: &str, column: &str) -> Result<bool> {
        self.any_row(
            "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
            params![table, column],
        )
        .await
    }

    /// Is the count from this `SELECT COUNT(*)` above zero.
    async fn any_row<P: libsql::params::IntoParams>(&self, sql: &str, params: P) -> Result<bool> {
        let mut rows = self.conn.query(sql, params).await?;
        Ok(match rows.next().await? {
            Some(row) => row.get::<i64>(0)? > 0,
            None => false,
        })
    }

    /// Channels and runs of the tag `from` go to this theme. With an empty `from`
    /// these are the rows from before themes; they go to the first theme of the
    /// config. Called on every start; it does nothing when no such rows are left.
    pub async fn adopt_theme(&self, from: &str, theme: &str) -> Result<()> {
        let channels = self
            .conn
            .execute(
                "UPDATE OR IGNORE channels SET theme = ?2 WHERE theme = ?1",
                params![from, theme],
            )
            .await?;
        self.conn
            .execute(
                "UPDATE runs SET theme = ?2 WHERE theme = ?1",
                params![from, theme],
            )
            .await?;
        if channels > 0 {
            tracing::info!(channels, %from, %theme, "channels moved to the theme");
        }
        Ok(())
    }

    // --------------------------------------------------------------- channels

    /// Debug commands (`preview`, `run --dry-run`) work on a copy: refreshing names there
    /// must not change the daemon database. The copy has the channels, all settings
    /// (the model from `/model`, the cached model lists, the `/reread` windows) and the
    /// topics of the last digest of each theme, so a dry run sees what a real run sees.
    pub async fn preview_copy(&self) -> Result<Self> {
        let copy = Self::open(":memory:").await?;

        let mut rows = self
            .conn
            .query("SELECT key, value FROM settings", ())
            .await?;
        while let Some(row) = rows.next().await? {
            let (key, value): (String, String) = (row.get(0)?, row.get(1)?);
            copy.set_setting(&key, &value).await?;
        }

        let mut rows = self
            .conn
            .query(
                "SELECT theme, digest_topics, started_at FROM runs r
                 WHERE status = 'ok' AND digest_topics IS NOT NULL
                   AND id = (SELECT MAX(id) FROM runs
                             WHERE theme = r.theme AND status = 'ok'
                               AND digest_topics IS NOT NULL)",
                (),
            )
            .await?;
        while let Some(row) = rows.next().await? {
            let (theme, topics, started_at): (String, String, String) =
                (row.get(0)?, row.get(1)?, row.get(2)?);
            copy.conn
                .execute(
                    "INSERT INTO runs (started_at, status, trigger, digest_topics, theme)
                     VALUES (?1, 'ok', 'copy', ?2, ?3)",
                    params![started_at, topics, theme],
                )
                .await?;
        }

        for channel in self.channels().await? {
            copy.conn.execute(
                "INSERT INTO channels (id, access_hash, username, title, enabled, last_msg_id, last_error, added_at, theme)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![channel.id, channel.access_hash, channel.username, channel.title,
                    channel.enabled as i64, channel.last_msg_id, channel.last_error, Utc::now().to_rfc3339(), channel.theme],
            ).await?;
        }
        Ok(copy)
    }

    /// All rows of all themes. A channel that is in two themes is here twice.
    pub async fn channels(&self) -> Result<Vec<Channel>> {
        self.query_channels("1 = 1 ORDER BY theme, title", params![])
            .await
    }

    /// The enabled rows of all themes: for work on the channels themselves (names),
    /// not on one theme.
    pub async fn enabled_channels(&self) -> Result<Vec<Channel>> {
        self.query_channels("enabled = 1 ORDER BY theme, title", params![])
            .await
    }

    /// The channels one theme reads.
    pub async fn theme_channels(&self, theme: &str) -> Result<Vec<Channel>> {
        self.query_channels("enabled = 1 AND theme = ?1 ORDER BY title", params![theme])
            .await
    }

    /// The channel itself, from any theme: the id and the access hash are the same
    /// in every row. For a cursor use `theme_channel`.
    pub async fn channel(&self, id: i64) -> Result<Option<Channel>> {
        Ok(self
            .query_channels("id = ?1 ORDER BY enabled DESC LIMIT 1", params![id])
            .await?
            .pop())
    }

    /// The row of the channel in one theme.
    pub async fn theme_channel(&self, theme: &str, id: i64) -> Result<Option<Channel>> {
        Ok(self
            .query_channels("theme = ?1 AND id = ?2", params![theme, id])
            .await?
            .pop())
    }

    async fn query_channels<P: libsql::params::IntoParams>(
        &self,
        filter: &str,
        params: P,
    ) -> Result<Vec<Channel>> {
        let rows = self
            .conn
            .prepare(&format!(
                "SELECT {CHANNEL_COLUMNS} FROM channels WHERE {filter}"
            ))
            .await?
            .query(params)
            .await?;
        collect_channels(rows).await
    }

    /// A channel from `/add`. Returns `true` if the cursor was reset: the channel is
    /// new or came back from the disabled ones (otherwise a channel disabled for a month
    /// would bring a month of posts when it comes back).
    pub async fn upsert_channel(
        &self,
        theme: &str,
        id: i64,
        access_hash: i64,
        username: Option<&str>,
        title: &str,
    ) -> Result<bool> {
        let existing = self.theme_channel(theme, id).await?;

        match existing {
            None => {
                self.conn
                    .execute(
                        "INSERT INTO channels
                           (id, access_hash, username, title, enabled, last_msg_id, added_at, theme)
                         VALUES (?1, ?2, ?3, ?4, 1, 0, ?5, ?6)",
                        params![
                            id,
                            access_hash,
                            username,
                            title,
                            Utc::now().to_rfc3339(),
                            theme
                        ],
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
                         WHERE id = ?1 AND theme = ?5",
                        params![id, access_hash, username, title, theme],
                    )
                    .await?;
                Ok(true)
            }
            Some(_) => {
                self.conn
                    .execute(
                        "UPDATE channels SET access_hash = ?2, username = ?3, title = ?4
                         WHERE id = ?1 AND theme = ?5",
                        params![id, access_hash, username, title, theme],
                    )
                    .await?;
                Ok(false)
            }
        }
    }

    pub async fn set_channel_error(&self, theme: &str, id: i64, error: Option<&str>) -> Result<()> {
        self.conn
            .execute(
                "UPDATE channels SET last_error = ?2 WHERE id = ?1 AND theme = ?3",
                params![id, error, theme],
            )
            .await?;
        Ok(())
    }

    /// Update only the title and the username, in every theme that has the channel.
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
    pub async fn disable_channel(&self, theme: &str, id: i64) -> Result<bool> {
        let affected = self
            .conn
            .execute(
                "UPDATE channels SET enabled = 0 WHERE id = ?1 AND theme = ?2",
                params![id, theme],
            )
            .await?;
        Ok(affected > 0)
    }

    /// Set the cursor exactly: `/add` puts it on the last message, so only posts that
    /// come after adding go into the digest.
    pub async fn set_cursor(&self, theme: &str, id: i64, last_msg_id: i32) -> Result<()> {
        self.conn
            .execute(
                "UPDATE channels SET last_msg_id = ?2 WHERE id = ?1 AND theme = ?3",
                params![id, last_msg_id, theme],
            )
            .await?;
        Ok(())
    }

    /// `/reset`: read the channel again from the `lookback_hours` window.
    pub async fn reset_channel(&self, theme: &str, id: i64) -> Result<bool> {
        let affected = self
            .conn
            .execute(
                "UPDATE channels SET last_msg_id = 0, last_error = NULL
                 WHERE id = ?1 AND theme = ?2",
                params![id, theme],
            )
            .await?;
        Ok(affected > 0)
    }

    /// The rows of a channel by `@username` or by numeric id — private channels have
    /// no username. One row per theme that has the channel. We also accept the
    /// `-100…` form: Telegram clients show it.
    pub async fn find_channels(&self, key: &str) -> Result<Vec<Channel>> {
        let key = key.trim();
        let all = self.channels().await?;
        if let Ok(id) = key.parse::<i64>() {
            let id = bare_channel_id(id);
            return Ok(all.into_iter().filter(|c| c.id == id).collect());
        }
        let name = key.trim_start_matches('@').to_lowercase();
        Ok(all
            .into_iter()
            .filter(|c| {
                c.username.as_deref().map(str::to_lowercase).as_deref() == Some(name.as_str())
            })
            .collect())
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

    /// Topics of the last delivered digest of the theme: its next run must not
    /// repeat them.
    pub async fn last_digest_topics(&self, theme: &str) -> Result<Vec<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT digest_topics FROM runs
                 WHERE status = 'ok' AND digest_topics IS NOT NULL AND theme = ?1
                 ORDER BY id DESC LIMIT 1",
                params![theme],
            )
            .await?;
        Ok(match rows.next().await? {
            Some(row) => serde_json::from_str(&row.get::<String>(0)?)
                .context("digest topics in the database are damaged")?,
            None => Vec::new(),
        })
    }

    pub async fn setting(&self, key: &str) -> Result<Option<String>> {
        let mut rows = self
            .conn
            .prepare("SELECT value FROM settings WHERE key = ?1")
            .await?
            .query(params![key])
            .await?;
        Ok(match rows.next().await? {
            Some(row) => Some(row.get(0)?),
            None => None,
        })
    }

    pub async fn delete_setting(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM settings WHERE key = ?1", params![key])
            .await?;
        Ok(())
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .await?;
        Ok(())
    }

    pub async fn start_run(&self, trigger: &str, theme: &str) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO runs (started_at, status, trigger, theme) VALUES (?1, ?2, ?3, ?4)",
                params![Utc::now().to_rfc3339(), STATUS_COLLECTING, trigger, theme],
            )
            .await?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The undelivered run of the theme (`summarized` or `publishing`): it blocks a
    /// new collection for this theme.
    pub async fn unfinished_theme_run(&self, theme: &str) -> Result<Option<Run>> {
        self.query_run(
            "SELECT * FROM runs WHERE status IN ('summarized','publishing') AND theme = ?1
             ORDER BY id LIMIT 1",
            params![theme],
        )
        .await
    }

    /// The oldest undelivered run of any theme.
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

    /// The last run of the theme, with any status — for `/status`.
    pub async fn last_theme_run(&self, theme: &str) -> Result<Option<Run>> {
        self.query_run(
            "SELECT * FROM runs WHERE theme = ?1 ORDER BY id DESC LIMIT 1",
            params![theme],
        )
        .await
    }

    /// The last run that reached the end: the scheduler counts catch-up from it.
    /// An empty run is also successful, or after a quiet day every restart
    /// would run the catch-up again.
    /// Per theme: one theme that finished must not hide the slot another theme missed.
    pub async fn last_finished_run(&self, theme: &str) -> Result<Option<Run>> {
        self.query_run(
            "SELECT * FROM runs WHERE status IN ('ok','empty') AND theme = ?1
             ORDER BY started_at DESC LIMIT 1",
            params![theme],
        )
        .await
    }

    /// The last published digest of the theme — for `/last`.
    pub async fn last_ok_run(&self, theme: &str) -> Result<Option<Run>> {
        self.query_run(
            "SELECT * FROM runs WHERE status = 'ok' AND theme = ?1 ORDER BY id DESC LIMIT 1",
            params![theme],
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
        chunks: &[NewChunk],
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

        for (idx, chunk) in chunks.iter().enumerate() {
            tx.execute(
                "INSERT INTO run_chunks (run_id, idx, html, random_id, photos)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    run_id,
                    idx as i64,
                    chunk.html.as_str(),
                    chunk.random_id,
                    chunk.photos as i64
                ],
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
                "SELECT idx, html, random_id, delivered_at, sent_msg_id, photos
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
                photos: row.get::<i64>(5)? as usize,
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
                "UPDATE channels SET last_msg_id = MAX(last_msg_id, ?2)
                 WHERE id = ?1 AND theme = ?3",
                params![*channel_id, *msg_id, run.theme.as_str()],
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
            theme: row.get(7)?,
        });
    }
    Ok(out)
}

/// `SELECT *` on `runs` — the column order is set by `SCHEMA`.
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
        // 16 is `digest_topics`, read only by `last_digest_topics`.
        theme: row.get(17)?,
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
