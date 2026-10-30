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
use config::Config;
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
    Login,
    /// Show how many posts were collected, without calling the LLM
    Preview,
    /// Build the digest and save it to out/digest.html, without posting anything
    Run {
        /// Required: only the daemon posts, so two processes do not post the digest twice
        #[arg(long)]
        dry_run: bool,
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
        Command::Login => {
            let session = telegram::client::connect(&cfg).await?;
            telegram::client::login(&session.client, &cfg).await
        }
        Command::Preview => preview(cfg).await,
        Command::Run { dry_run } => {
            if !dry_run {
                bail!(
                    "only the daemon posts: `serve` on schedule or /run to the bot. \
                     To check, use `run --dry-run`"
                );
            }
            dry_run_once(cfg).await
        }
        Command::Serve => serve(cfg).await,
    }
}

/// A cheap check of the selection: channels, cursors, album merging — all without
/// a single Anthropic token.
async fn preview(cfg: Arc<Config>) -> Result<()> {
    let runner = prepare(cfg.clone()).await?;
    runner.refresh_channels().await?;

    let channels = runner.db.enabled_channels().await?;
    let collected =
        telegram::client::collect_posts(&runner.client, &cfg, &channels, &Cancel::never()).await?;

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
    Ok(())
}

/// A run without posting and without writing to the database: cursors do not move,
/// no delivery journal is created.
async fn dry_run_once(cfg: Arc<Config>) -> Result<()> {
    let runner = prepare(cfg.clone()).await?;
    runner.refresh_channels().await?;

    let channels = runner.db.enabled_channels().await?;
    let collected =
        telegram::client::collect_posts(&runner.client, &cfg, &channels, &Cancel::never()).await?;

    if collected.posts.is_empty() {
        println!("no new posts, nothing to build the digest from");
        return Ok(());
    }

    let summarizer = Summarizer::new(&cfg)?;
    let summary = summarizer
        .summarize(
            &cfg.prompts,
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
            digest_title: &cfg.prompts.digest_title,
            tldr_title: &cfg.prompts.tldr_title,
            timezone: runner.timezone(),
            now: Utc::now(),
        },
    );

    if chunks.is_empty() {
        println!("the model found no topics — there would be nothing to post");
        return Ok(());
    }

    std::fs::create_dir_all("out")?;
    let html = chunks.join("\n\n<hr/>\n\n");
    std::fs::write("out/digest.html", &html).context("could not write out/digest.html")?;
    println!("{html}");
    tracing::info!(
        topics = result.topics.len(),
        parts = chunks.len(),
        "dry-run: saved to out/digest.html, state not changed"
    );
    Ok(())
}

/// Common setup for the debug commands: database, client, login check.
/// They do not need a shutdown token — the process runs once.
async fn prepare(cfg: Arc<Config>) -> Result<Arc<Runner>> {
    let db = Arc::new(Db::open(&cfg.db_file).await?.preview_copy().await?);
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

    let interrupted = db.close_interrupted_runs().await?;
    if interrupted > 0 {
        tracing::warn!(
            interrupted,
            "runs that stopped during collection are closed"
        );
    }

    // The personal account reads the channels; it is also the owner of the bot.
    let session = telegram::client::connect(&cfg).await?;
    telegram::client::ensure_authorized(&session.client).await?;
    let owner = session
        .client
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
        session.client,
        Some(bot),
        cancel.clone(),
    ));

    // The command listener and the scheduler are two tasks. A revoked session breaks
    // either of them, the process exits with a non-zero code, Docker restarts it, and
    // the logs say what to do (`login`).
    let listener = telegram::commands::listen(runner.clone(), bot_updates, cancel.clone());

    tracing::info!(cron = %cfg.schedule.cron, tz = %cfg.schedule.timezone, "daemon started");

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
        report(runner.trigger(TRIGGER_CATCH_UP).await?);
    } else if runner.cfg.schedule.run_on_start {
        tracing::info!("run_on_start: start run");
        report(runner.trigger(TRIGGER_CRON).await?);
    } else if runner.cfg.schedule.catch_up && scheduler.missed_since(last_finished, Utc::now())? {
        tracing::info!(?last_finished, "missed slot, catching up");
        report(runner.trigger(TRIGGER_CATCH_UP).await?);
    }

    loop {
        let next = scheduler.next_after(Utc::now())?;
        tracing::info!(next = %next, "waiting for the next run");

        if wait_until(next, &cancel).await == Wake::Interrupted {
            tracing::info!("shutdown signal received, exiting");
            return Ok(());
        }

        // An error in one run must not break the daemon: it is already saved in the
        // database and sent as a notification, we wait for the next slot. Only
        // fatal errors go out — for example, a revoked session.
        report(runner.trigger(TRIGGER_CRON).await?);
    }
}

/// A missed slot is a warn, not an info: the owner notices a silently missed
/// digest only a day later.
fn report(outcome: run::Outcome) {
    use run::Outcome;
    match outcome {
        Outcome::Busy(_) | Outcome::Stale(_) | Outcome::Pending(..) | Outcome::Failed(..) => {
            tracing::warn!("{outcome}")
        }
        Outcome::Delivered(_) | Outcome::Finished { .. } | Outcome::Abandoned(_) => {
            tracing::info!("{outcome}")
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
