//! tg-digest — читает Telegram-каналы, суммаризирует их через Claude и постит
//! дайджест обратно в Telegram по cron-расписанию из конфига.
//!
//! Штатный режим — `serve` в контейнере: планировщик и слушатель команд в
//! «Избранном» живут в одном процессе, каналы и курсоры — в базе. Остальные
//! команды CLI отладочные: `login` один раз и `preview` / `run --dry-run`,
//! чтобы проверить выборку и промпт, не публикуя ничего.

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

/// Как часто просыпаемся, чтобы проверить, не наступил ли слот.
///
/// Один длинный sleep до времени запуска непредсказуемо переживает засыпание
/// машины, поэтому ждём тиками и каждый раз сверяемся с часами.
const TICK: Duration = Duration::from_secs(60);

#[derive(Parser)]
#[command(name = "tg-digest", about = "Дайджест Telegram-каналов через Claude")]
struct Cli {
    /// Путь к конфигу
    #[arg(long, short, default_value = "/data/config.toml", global = true)]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Разовая авторизация в Telegram (код из приложения, при 2FA — пароль)
    Login,
    /// Показать, сколько постов собралось, без вызова LLM
    Preview,
    /// Собрать дайджест и сохранить в out/digest.html, ничего не публикуя
    Run {
        /// Обязателен: публикует только демон, чтобы два процесса не дублировали дайджест
        #[arg(long)]
        dry_run: bool,
    },
    /// Демон: прогоны по расписанию и команды из «Избранного»
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
                    "публикует только демон: `serve` по расписанию или /run в «Избранном». \
                     Для проверки — `run --dry-run`"
                );
            }
            dry_run_once(cfg).await
        }
        Command::Serve => serve(cfg).await,
    }
}

/// Дешёвая проверка выборки: каналы, курсоры, склейка альбомов — всё без
/// единого токена в Anthropic.
async fn preview(cfg: Arc<Config>) -> Result<()> {
    let runner = prepare(cfg.clone()).await?;
    runner.refresh_channels().await?;

    let channels = runner.db.enabled_channels().await?;
    let collected =
        telegram::client::collect_posts(&runner.client, &cfg, &channels, &Cancel::never()).await?;

    println!(
        "Каналов включено: {}, собрано {} постов\n",
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
        println!("! {} исключён: {reason}", channel.label());
    }
    Ok(())
}

/// Прогон без публикации и без записи в базу: курсоры не двигаются, журнал
/// доставки не создаётся.
async fn dry_run_once(cfg: Arc<Config>) -> Result<()> {
    let runner = prepare(cfg.clone()).await?;
    runner.refresh_channels().await?;

    let channels = runner.db.enabled_channels().await?;
    let collected =
        telegram::client::collect_posts(&runner.client, &cfg, &channels, &Cancel::never()).await?;

    if collected.posts.is_empty() {
        println!("новых постов нет, дайджест собирать не из чего");
        return Ok(());
    }

    let summarizer = Summarizer::new(&cfg.anthropic)?;
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
        println!("модель не нашла ни одной темы — публиковать было бы нечего");
        return Ok(());
    }

    std::fs::create_dir_all("out")?;
    let html = chunks.join("\n\n<hr/>\n\n");
    std::fs::write("out/digest.html", &html).context("не удалось записать out/digest.html")?;
    println!("{html}");
    tracing::info!(
        topics = result.topics.len(),
        parts = chunks.len(),
        "dry-run: сохранено в out/digest.html, состояние не изменено"
    );
    Ok(())
}

/// Общая подготовка отладочных команд: база, клиент, проверка авторизации,
/// seed каналов. Токен остановки им не нужен — процесс одноразовый.
async fn prepare(cfg: Arc<Config>) -> Result<Arc<Runner>> {
    let db = Arc::new(Db::open(&cfg.db_file).await?.preview_copy().await?);
    let session = telegram::client::connect(&cfg).await?;
    telegram::client::ensure_authorized(&session.client).await?;

    seed_if_empty(&cfg, &db, &session.client).await?;

    Ok(Arc::new(Runner::new(
        cfg,
        db,
        session.client,
        Cancel::never(),
    )))
}

/// `[[channels]]` из конфига — только seed для пустой базы: дальше список живёт
/// в папке Telegram и в базе.
async fn seed_if_empty(
    cfg: &Config,
    db: &Db,
    client: &grammers_client::client::Client,
) -> Result<()> {
    if !db.channels().await?.is_empty() {
        return Ok(());
    }
    let seeds = cfg.seed_channels();
    if seeds.is_empty() {
        return Ok(());
    }
    let added = telegram::folder::seed(client, db, &seeds).await?;
    tracing::info!(added, "каналы из конфига перенесены в базу");
    Ok(())
}

async fn serve(cfg: Arc<Config>) -> Result<()> {
    let scheduler = Scheduler::new(&cfg.schedule.cron, &cfg.schedule.timezone)?;
    let db = Arc::new(Db::open(&cfg.db_file).await?);

    let interrupted = db.close_interrupted_runs().await?;
    if interrupted > 0 {
        tracing::warn!(interrupted, "прогоны, оборвавшиеся на сборе, закрыты");
    }

    let session = telegram::client::connect(&cfg).await?;
    telegram::client::ensure_authorized(&session.client).await?;
    seed_if_empty(&cfg, &db, &session.client).await?;

    let (source, cancel) = cancel::channel();
    tokio::spawn(watch_signals(source));

    let runner = Arc::new(Runner::new(
        cfg.clone(),
        db.clone(),
        session.client,
        cancel.clone(),
    ));

    // Слушатель команд и планировщик — две задачи над одним клиентом. Отозванная
    // сессия роняет любую из них, процесс выходит с ненулевым кодом, Docker его
    // перезапускает, а в логах написано, что делать (`login`).
    let listener = telegram::commands::listen(runner.clone(), session.updates, cancel.clone());

    tracing::info!(cron = %cfg.schedule.cron, tz = %cfg.schedule.timezone, "демон запущен");

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
        tracing::info!("восстанавливаем недоставленный прогон при старте");
        report(runner.trigger(TRIGGER_CATCH_UP).await?);
    } else if runner.cfg.schedule.run_on_start {
        tracing::info!("run_on_start: стартовый прогон");
        report(runner.trigger(TRIGGER_CRON).await?);
    } else if runner.cfg.schedule.catch_up && scheduler.missed_since(last_finished, Utc::now())? {
        tracing::info!(?last_finished, "пропущенный слот, догоняем");
        report(runner.trigger(TRIGGER_CATCH_UP).await?);
    }

    loop {
        let next = scheduler.next_after(Utc::now())?;
        tracing::info!(next = %next, "ждём следующего прогона");

        if wait_until(next, &cancel).await == Wake::Interrupted {
            tracing::info!("получен сигнал остановки, выходим");
            return Ok(());
        }

        // Ошибка одного прогона не должна ронять демон: она уже записана в базу
        // и ушла уведомлением, ждём следующего слота. Наружу летит только
        // неустранимое — например, отозванная сессия.
        report(runner.trigger(TRIGGER_CRON).await?);
    }
}

/// Пропущенный слот — это warn, а не info: тихо пропущенный дайджест владелец
/// заметит только через сутки.
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

/// SIGTERM от `docker stop` и Ctrl+C — один и тот же токен. Второй сигнал —
/// немедленный выход: значит, владелец не готов ждать `stop_grace_period`.
async fn watch_signals(source: cancel::CancelSource) {
    first_signal().await;
    tracing::info!("получен сигнал остановки, завершаем текущую работу");
    source.cancel();

    first_signal().await;
    tracing::warn!("второй сигнал — выходим немедленно");
    std::process::exit(130);
}

#[cfg(unix)]
async fn first_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        Err(e) => {
            tracing::warn!(error = %e, "SIGTERM недоступен, ждём только Ctrl+C");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_waits_for_publication_owned_by_either_loop() {
        for publication_in_scheduler in [true, false] {
            let (finished, recorded) = tokio::sync::oneshot::channel();
            let publication = async {
                tokio::task::yield_now().await;
                finished.send(()).unwrap();
                Ok(())
            };
            let idle = async { Ok(()) };
            if publication_in_scheduler {
                finish_service_loops(publication, idle).await.unwrap();
            } else {
                finish_service_loops(idle, publication).await.unwrap();
            }
            recorded
                .await
                .expect("delivery must be recorded before exit");
        }
    }

    #[tokio::test]
    async fn fatal_loop_error_stops_the_service() {
        let result = finish_service_loops(std::future::pending(), async {
            anyhow::bail!("session revoked")
        })
        .await;
        assert!(result.is_err());
    }
}
