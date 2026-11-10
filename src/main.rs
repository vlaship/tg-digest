//! tg-digest — reads Telegram channels, summarizes them with Claude and posts
//! a digest back to Telegram on the cron schedule from the config.
//!
//! The normal mode is `serve` in a container: the scheduler and the bot that listens
//! for commands live in one process, channels and cursors in the
//! database. The other CLI commands are for debugging: `login` once, and `preview` /
//! `run --dry-run` to check the post selection and the prompt without posting anything.

mod cancel;
mod config;
mod db;
mod digest;
mod models;
mod run;
mod schedule;
mod summarize;
mod telegram;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cancel::Cancel;
use config::{Config, Theme};
use db::Db;
use digest::RenderOptions;
use run::{Runner, TRIGGER_CATCH_UP, TRIGGER_CRON};
use schedule::Scheduler;
use summarize::Summarizer;

/// How often we wake up to check whether a slot has come.
///
/// One long sleep until the run time survives a sleeping machine in
/// unpredictable ways, so we wait in ticks and check the clock each time.
const TICK: Duration = Duration::from_secs(60);

/// Version of this build: the UTC build time, the same as the Docker image tag
/// (`20261103-041200`), and the short git commit. CI passes both as `BUILD_TIME`
/// and `GIT_SHA`; a build without them is "dev".
pub fn version() -> String {
    let version = option_env!("BUILD_TIME")
        .filter(|time| !time.is_empty())
        .unwrap_or("dev");
    match option_env!("GIT_SHA").filter(|sha| !sha.is_empty()) {
        Some(sha) => format!("{version} ({})", &sha[..sha.len().min(7)]),
        None => version.to_string(),
    }
}

#[derive(Parser)]
#[command(name = "tg-digest", about = "Digest of Telegram channels with Claude")]
struct Cli {
    /// Path to the config
    #[arg(long, short, default_value = "/data/config.toml", global = true)]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// One-time login to Telegram (code from the app, and the password with 2FA)
    Login {
        /// Log in with a QR code scanned in the Telegram app, no code is sent
        #[arg(long)]
        qr: bool,
    },
    /// Show how many posts were collected, without calling the LLM
    Preview {
        /// Only this theme (a tag from [[themes]]); without it, all themes
        #[arg(long)]
        theme: Option<String>,
    },
    /// Build the digest and save it to out/digest.html, without posting anything
    Run {
        /// Required: only the daemon posts, so two processes do not post the digest twice
        #[arg(long)]
        dry_run: bool,
        /// Only this theme (a tag from [[themes]]); without it, all themes
        #[arg(long)]
        theme: Option<String>,
    },
    /// Daemon: runs on schedule and commands to the bot
    Serve,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tg_digest=info,warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let cfg = Arc::new(Config::load(&cli.config)?);

    match cli.command {
        Command::Login { qr } => {
            let session = telegram::client::connect(&cfg).await?;
            if qr {
                telegram::client::login_qr(&session, &cfg).await
            } else {
                telegram::client::login(&session.client, &cfg).await
            }
        }
        Command::Preview { theme } => {
            let themes = chosen_themes(&cfg, theme.as_deref())?;
            let runner = prepare(cfg.clone()).await?;
            runner.refresh_channels().await?;
            for theme in &themes {
                preview(&runner, theme).await?;
            }
            Ok(())
        }
        Command::Run { dry_run, theme } => {
            if !dry_run {
                bail!(
                    "only the daemon posts: `serve` on schedule or /run to the bot. \
                     To check, use `run --dry-run`"
                );
            }
            let themes = chosen_themes(&cfg, theme.as_deref())?;
            let runner = prepare(cfg.clone()).await?;
            runner.refresh_channels().await?;
            for theme in &themes {
                dry_run_once(&runner, theme, themes.len() > 1).await?;
            }
            Ok(())
        }
        Command::Serve => serve(cfg).await,
    }
}

/// The themes a debug command works on: the one from `--theme`, or all of them.
fn chosen_themes(cfg: &Config, tag: Option<&str>) -> Result<Vec<Theme>> {
    match tag {
        None => Ok(cfg.themes()),
        Some(tag) => {
            let known: Vec<String> = cfg.themes().into_iter().map(|t| t.tag).collect();
            let theme = cfg
                .theme(tag)
                .with_context(|| format!("there is no theme {tag:?}, the config has {known:?}"))?;
            Ok(vec![theme])
        }
    }
}

/// A cheap check of the selection: channels, cursors, album merging — all without
/// a single Anthropic token.
async fn preview(runner: &Runner, theme: &Theme) -> Result<()> {
    let cfg = &runner.cfg;
    let channels = runner.db.theme_channels(&theme.tag).await?;
    println!("== theme {} ==", theme.tag);
    let collected = telegram::client::collect_posts(
        &runner.client,
        cfg,
        &channels,
        cfg.telegram.lookback_hours,
        &Cancel::never(),
    )
    .await?;

    println!(
        "Channels enabled: {}, collected {} posts\n",
        channels.len(),
        collected.posts.len()
    );
    for post in &collected.posts {
        let first_line = post
            .text
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(90)
            .collect::<String>();
        println!(
            "[{}] {} — {}",
            post.title,
            post.date.format("%d.%m %H:%M"),
            first_line
        );
    }
    for (channel, reason) in &collected.failures {
        println!("! {} excluded: {reason}", channel.label());
    }
    println!();
    Ok(())
}

/// A run without posting and without writing to the database: cursors do not move,
/// no delivery journal is created. With several themes each one gets its own file,
/// `out/digest-<tag>.html`.
async fn dry_run_once(runner: &Runner, theme: &Theme, many: bool) -> Result<()> {
    let cfg = &runner.cfg;
    let channels = runner.db.theme_channels(&theme.tag).await?;
    let collected = telegram::client::collect_posts(
        &runner.client,
        cfg,
        &channels,
        cfg.telegram.lookback_hours,
        &Cancel::never(),
    )
    .await?;

    if collected.posts.is_empty() {
        println!(
            "{}: no new posts, nothing to build the digest from",
            theme.tag
        );
        return Ok(());
    }

    let summarizer = Summarizer::new(&runner.catalog.spec(&runner.model().await).await?)?;
    let summary = summarizer
        .summarize(
            &theme.prompts,
            &collected.posts,
            cfg.telegram.lookback_hours,
            channels.len(),
            runner.timezone(),
            &Cancel::never(),
        )
        .await?;

    let mut result = summary.digest;
    digest::drop_foreign_sources(&mut result, &collected.posts);

    let chunks = digest::render_html(
        &result,
        &RenderOptions {
            digest_title: &theme.prompts.digest_title,
            timezone: runner.timezone(),
            now: Utc::now(),
        },
    );

    if chunks.is_empty() {
        println!(
            "{}: the model found no topics — there would be nothing to post",
            theme.tag
        );
        return Ok(());
    }

    std::fs::create_dir_all("out")?;
    let file = if many {
        format!("out/digest-{}.html", theme.tag)
    } else {
        "out/digest.html".to_string()
    };
    let html = chunks.join("\n\n<hr/>\n\n");
    std::fs::write(&file, &html).with_context(|| format!("could not write {file}"))?;
    println!("{html}");
    tracing::info!(
        theme = %theme.tag,
        topics = result.topics.len(),
        parts = chunks.len(),
        %file,
        "dry-run: saved, state not changed"
    );
    Ok(())
}

/// Channels that no theme of the config owns, but that are not lost: rows from
/// before themes, and rows of the default theme after the owner added `[[themes]]`.
/// They go to the first theme.
async fn adopt_old_channels(db: &Db, cfg: &Config) -> Result<()> {
    let first = cfg.themes().remove(0).tag;
    db.adopt_theme("", &first).await?;
    if cfg.theme(config::DEFAULT_THEME).is_none() {
        db.adopt_theme(config::DEFAULT_THEME, &first).await?;
    }
    Ok(())
}

/// Common setup for the debug commands: database, client, login check.
/// They do not need a shutdown token — the process runs once.
async fn prepare(cfg: Arc<Config>) -> Result<Arc<Runner>> {
    let db = Arc::new(Db::open(&cfg.db_file).await?.preview_copy().await?);
    adopt_old_channels(&db, &cfg).await?;
    let session = telegram::client::connect(&cfg).await?;
    telegram::client::ensure_authorized(&session.client).await?;

    // No bot: the debug commands never send anything.
    Ok(Arc::new(Runner::new(
        cfg,
        db,
        session.client,
        None,
        Cancel::never(),
    )))
}

async fn serve(cfg: Arc<Config>) -> Result<()> {
    let scheduler = Scheduler::new(&cfg.schedule.cron, &cfg.schedule.timezone)?;
    let db = Arc::new(Db::open(&cfg.db_file).await?);
    adopt_old_channels(&db, &cfg).await?;
    // A theme that was removed from the config leaves its channels behind: nothing
    // reads them, so say it.
    let mut lost: Vec<String> = Vec::new();
    for channel in db.enabled_channels().await? {
        if cfg.theme(&channel.theme).is_none() && !lost.contains(&channel.theme) {
            lost.push(channel.theme);
        }
    }
    if !lost.is_empty() {
        tracing::warn!(themes = ?lost, "channels of themes that are not in the config are not read");
    }

    let interrupted = db.close_interrupted_runs().await?;
    if interrupted > 0 {
        tracing::warn!(
            interrupted,
            "runs that stopped during collection are closed"
        );
    }

    // The personal account reads the channels; it is also the owner of the bot.
    let telegram::client::Session {
        client, updates, ..
    } = telegram::client::connect(&cfg).await?;
    // The personal account only reads; nobody listens to its updates. The channel is
    // unbounded, so a kept receiver collects every update of the account (all chats
    // and channels) until the memory runs out. Drop it: grammers then throws the
    // updates away.
    drop(updates);
    telegram::client::ensure_authorized(&client).await?;
    let owner = client
        .get_me()
        .await
        .context("could not get own profile")?
        .id();

    // The bot talks to the owner: commands, replies, digests, notifications.
    let (bot, bot_updates) = telegram::bot::connect(&cfg, owner).await?;

    let (source, cancel) = cancel::channel();
    tokio::spawn(watch_signals(source));

    let runner = Arc::new(Runner::new(
        cfg.clone(),
        db.clone(),
        client,
        Some(bot),
        cancel.clone(),
    ));

    // The command listener and the scheduler are two tasks. A revoked session breaks
    // either of them, the process exits with a non-zero code, Docker restarts it, and
    // the logs say what to do (`login`).
    let listener = telegram::commands::listen(runner.clone(), bot_updates, cancel.clone());

    tracing::info!(version = %version(), cron = %cfg.schedule.cron, tz = %cfg.schedule.timezone, "daemon started");

    let schedule_loop = schedule_loop(runner, scheduler, cancel);

    // Normal shutdown must wait for both loops: either may own a publication.
    finish_service_loops(schedule_loop, listener).await
}

async fn finish_service_loops(
    scheduler: impl std::future::Future<Output = Result<()>>,
    listener: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    tokio::try_join!(scheduler, listener)?;
    Ok(())
}

async fn schedule_loop(runner: Arc<Runner>, scheduler: Scheduler, cancel: Cancel) -> Result<()> {
    let last_finished = runner
        .db
        .last_finished_run()
        .await?
        .map(|run| run.started_at);

    if runner.db.unfinished_run().await?.is_some() {
        tracing::info!("recovering an undelivered run on start");
        finish(&runner, TRIGGER_CATCH_UP, runner.recover().await).await?;
    } else if runner.cfg.schedule.run_on_start {
        tracing::info!("run_on_start: start run");
        fire(&runner, TRIGGER_CRON).await?;
    } else if runner.cfg.schedule.catch_up && scheduler.missed_since(last_finished, Utc::now())? {
        tracing::info!(?last_finished, "missed slot, catching up");
        fire(&runner, TRIGGER_CATCH_UP).await?;
    }

    loop {
        let next = scheduler.next_after(Utc::now())?;
        tracing::info!(next = %next, "waiting for the next run");

        if wait_until(next, &cancel).await == Wake::Interrupted {
            tracing::info!("shutdown signal received, exiting");
            return Ok(());
        }

        fire(&runner, TRIGGER_CRON).await?;
    }
}

/// One scheduled run. An error in one run must not break the daemon: we tell the
/// owner and wait for the next slot. Only fatal errors go out — for example, a
/// revoked session. Otherwise Docker restarts the daemon, `run_on_start` or
/// `catch_up` starts the same run again, and every restart pays for the model.
async fn fire(runner: &Runner, trigger: &str) -> Result<()> {
    finish(runner, trigger, runner.trigger(trigger, None).await).await
}

async fn finish(runner: &Runner, trigger: &str, result: Result<run::Outcomes>) -> Result<()> {
    match result {
        Ok(outcomes) => outcomes.0.into_iter().for_each(report),
        Err(e) if telegram::client::is_fatal(&e) => return Err(e),
        Err(e) => {
            let reason = format!("{e:#}");
            tracing::error!(%trigger, %reason, "run failed");
            let short: String = reason.chars().take(300).collect();
            runner.notify(&format!("⚠️ The run failed: {short}")).await;
        }
    }
    Ok(())
}

/// A missed slot is a warn, not an info: the owner notices a silently missed
/// digest only a day later.
fn report((theme, outcome): (String, run::Outcome)) {
    use run::Outcome;
    match outcome {
        Outcome::Busy(_) | Outcome::Stale(_) | Outcome::Pending(..) | Outcome::Failed(..) => {
            tracing::warn!(%theme, "{outcome}")
        }
        Outcome::Delivered(_) | Outcome::Finished { .. } | Outcome::Abandoned(_) => {
            tracing::info!(%theme, "{outcome}")
        }
    }
}

#[derive(PartialEq)]
enum Wake {
    Due,
    Interrupted,
}

async fn wait_until(deadline: chrono::DateTime<Utc>, cancel: &Cancel) -> Wake {
    loop {
        if cancel.is_cancelled() {
            return Wake::Interrupted;
        }
        if Utc::now() >= deadline {
            return Wake::Due;
        }
        if cancel.sleep(TICK).await.is_err() {
            return Wake::Interrupted;
        }
    }
}

/// SIGTERM from `docker stop` and Ctrl+C are the same token. A second signal means
/// exit at once: the owner does not want to wait for `stop_grace_period`.
async fn watch_signals(source: cancel::CancelSource) {
    first_signal().await;
    tracing::info!("shutdown signal received, finishing the current work");
    source.cancel();

    first_signal().await;
    tracing::warn!("second signal — exiting at once");
    std::process::exit(130);
}

#[cfg(unix)]
async fn first_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        Err(e) => {
            tracing::warn!(error = %e, "SIGTERM is not available, waiting only for Ctrl+C");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(not(unix))]
async fn first_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
