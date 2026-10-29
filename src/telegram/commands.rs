//! Управление сервисом командами в «Избранном».
//!
//! Слушаем updates и реагируем **только** на исходящие сообщения в чате с самим
//! собой: никто, кроме владельца аккаунта, туда написать не может, поэтому
//! отдельная авторизация не нужна. Бот для этого не заводится — он всё равно не
//! умеет читать каналы, а UI для одного владельца даёт сам аккаунт.
//!
//! Каналы адресуются по `@username` или по числовому `id`: у приватных каналов
//! username нет, поэтому `/list` и ответ на `/add` всегда показывают `id`.

use anyhow::{bail, Context, Result};
use grammers_client::client::UpdatesConfiguration;
use grammers_client::message::InputMessage;
use grammers_client::peer::Peer;
use grammers_client::update::Update;
use grammers_session::updates::UpdatesLike;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinSet;

use crate::cancel::Cancel;
use crate::db::Channel;
use crate::run::{Runner, TRIGGER_MANUAL};
use crate::telegram::client::is_auth_error;
use crate::telegram::{folder, publish};

const HELP: &str = "\
/add @канал | ссылка | id — добавить канал
/rm @канал | id — убрать канал
/list — каналы, курсоры, ошибки
/reset id — перечитать канал с окна lookback_hours
/run — прогон сейчас (или доставка недоставленного)
/status — последний прогон: статус, посты, токены
/last — переотправить последний дайджест
/resume N — доставить устаревший прогон (возможен дубль)
/abandon N — снять недоставленный прогон
/sync — перечитать папку
/help — эта справка";

/// Крутить цикл updates, пока не придёт сигнал остановки.
pub async fn listen(
    runner: Arc<Runner>,
    updates: UnboundedReceiver<UpdatesLike>,
    cancel: Cancel,
) -> Result<()> {
    let me = runner
        .client
        .get_me()
        .await
        .context("не удалось получить свой профиль")?
        .id();

    let mut stream = runner
        .client
        .stream_updates(
            updates,
            UpdatesConfiguration {
                catch_up: false,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("не удалось подписаться на updates: {e}"))?;

    tracing::info!("слушаем команды в «Избранном», /help — список");
    let mut commands = JoinSet::<Result<()>>::new();

    loop {
        let update = tokio::select! {
            _ = cancel.cancelled() => {
                // Let an in-flight send record its delivery before exiting.
                while let Some(result) = commands.join_next().await {
                    result.context("обработчик команды упал")??;
                }
                tracing::info!("слушатель команд остановлен");
                return Ok(());
            }
            result = commands.join_next(), if !commands.is_empty() => {
                result.expect("есть обработчик").context("обработчик команды упал")??;
                continue;
            }
            update = stream.next() => update,
        };

        let update = match update {
            Ok(update) => update,
            // Отозванная сессия роняет цикл: это неустранимое, а не «ошибка
            // одного update».
            Err(e) if is_auth_error(&e) => {
                return Err(anyhow::Error::new(e).context("сессия Telegram недействительна"))
            }
            Err(e) => {
                tracing::warn!(error = %e, "ошибка чтения updates");
                continue;
            }
        };

        let Update::NewMessage(message) = update else {
            continue;
        };
        if !message.outgoing() || message.peer_id() != me {
            continue;
        }
        let text = message.text().trim().to_string();
        if !text.starts_with('/') {
            continue;
        }

        tracing::info!(command = %text, "команда из «Избранного»");
        let runner = runner.clone();
        let cancel = cancel.clone();
        commands.spawn(async move {
            let reply = match handle(&runner, &text).await {
                Ok(reply) => reply,
                Err(e) if crate::telegram::client::is_fatal(&e) => return Err(e),
                // Ошибку команды показываем владельцу, а не прячем в лог.
                Err(e) => format!("не получилось: {e:#}"),
            };

            // Длинный ответ (`/list` на полусотне каналов) Telegram не обрежет,
            // а отвергнет целиком — режем сами.
            for piece in crate::digest::split_plain(&reply) {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                if let Err(e) = message
                    .respond(InputMessage::new().text(piece).link_preview(false))
                    .await
                {
                    if is_auth_error(&e) {
                        return Err(anyhow::Error::new(e)
                            .context("сессия Telegram недействительна; выполните login"));
                    }
                    tracing::warn!(error = %e, "не удалось ответить на команду");
                    break;
                }
            }
            Ok(())
        });
    }
}

async fn handle(runner: &Arc<Runner>, text: &str) -> Result<String> {
    let mut parts = text.splitn(2, char::is_whitespace);
    let command = parts.next().unwrap_or_default();
    let arg = parts.next().unwrap_or("").trim();

    match command {
        "/help" | "/start" => Ok(HELP.into()),
        "/list" => list(runner).await,
        "/status" => status(runner).await,
        "/run" => Ok(runner.trigger(TRIGGER_MANUAL).await?.to_string()),
        "/resume" => match arg.parse::<i64>() {
            Ok(id) => Ok(runner.resume(id).await?.to_string()),
            Err(_) => Ok("нужен номер прогона: /resume 12".into()),
        },
        "/abandon" => match arg.parse::<i64>() {
            Ok(id) => Ok(runner.abandon(id).await?.to_string()),
            Err(_) => Ok("нужен номер прогона: /abandon 12".into()),
        },
        // Футуры ленивые: действие не начнётся, пока `guarded` не пропустит.
        "/sync" => guarded(runner, sync(runner)).await,
        "/add" => guarded(runner, add(runner, arg)).await,
        "/rm" => guarded(runner, remove(runner, arg)).await,
        "/reset" => guarded(runner, reset(runner, arg)).await,
        "/last" => guarded(runner, last(runner)).await,
        other => Ok(format!("не знаю команду {other}\n\n{HELP}")),
    }
}

/// Команды, которые трогают курсоры, идут под той же блокировкой, что и прогон,
/// и отклоняются, если есть недоставленный прогон: его `runs.cursors` при
/// доставке перезапишут любой сброс.
async fn guarded<Fut>(runner: &Arc<Runner>, action: Fut) -> Result<String>
where
    Fut: std::future::Future<Output = Result<String>>,
{
    let Some(_guard) = runner.try_lock() else {
        let id = runner
            .db
            .last_run()
            .await?
            .map(|r| r.id)
            .unwrap_or_default();
        return Ok(format!("идёт прогон #{id}, повторите после"));
    };
    if let Some(run) = runner.db.unfinished_run().await? {
        return Ok(format!(
            "есть недоставленный прогон #{} — /run для доставки или /abandon {}",
            run.id, run.id
        ));
    }
    action.await
}

async fn list(runner: &Arc<Runner>) -> Result<String> {
    let channels = runner.db.channels().await?;
    if channels.is_empty() {
        return Ok("каналов нет: соберите папку в Telegram и вызовите /sync".into());
    }

    let mut out = String::new();
    for channel in &channels {
        out.push_str(&format!(
            "{} {} — id {}, {}, курсор {}{}\n",
            if channel.enabled { "✅" } else { "⛔" },
            channel.title,
            channel.id,
            channel
                .username
                .as_ref()
                .map(|u| format!("@{u}"))
                .unwrap_or_else(|| "приватный".into()),
            channel.last_msg_id,
            channel
                .last_error
                .as_ref()
                .map(|e| format!("\n   ошибка: {e}"))
                .unwrap_or_default(),
        ));
    }
    Ok(out)
}

async fn status(runner: &Arc<Runner>) -> Result<String> {
    let Some(run) = runner.db.last_run().await? else {
        return Ok("прогонов ещё не было".into());
    };

    let chunks = runner.db.chunks(run.id).await?;
    let delivered = chunks.iter().filter(|c| c.delivered_at.is_some()).count();

    let mut out = format!(
        "прогон #{} ({}): {}\nначат {}\nзавершён {}\nпостов {}, тем {}\nтокены: {} / {}\nчасти: {delivered} из {}\nадресат: {}",
        run.id,
        run.trigger,
        run.status,
        run.started_at.format("%Y-%m-%d %H:%M UTC"),
        run.finished_at
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "—".into()),
        run.posts.unwrap_or(0),
        run.topics.unwrap_or(0),
        run.input_tokens.unwrap_or(0),
        run.output_tokens.unwrap_or(0),
        chunks.len(),
        run.dest_label.as_deref().unwrap_or("—"),
    );
    if let Some(error) = &run.error {
        out.push_str(&format!("\nошибка: {error}"));
    }

    // Часть с `delivered_at`, но без `sent_msg_id` — это доставка, за которую
    // Telegram не вернул id (или ответил RANDOM_ID_DUPLICATE). Так и пишем.
    for chunk in &chunks {
        out.push_str(&match (&chunk.delivered_at, chunk.sent_msg_id) {
            (Some(at), Some(id)) => format!(
                "\n  часть {}: доставлена {} (msg {id})",
                chunk.idx + 1,
                at.format("%H:%M")
            ),
            (Some(at), None) => format!(
                "\n  часть {}: доставлена {} (id сообщения не пришёл)",
                chunk.idx + 1,
                at.format("%H:%M")
            ),
            (None, _) => format!("\n  часть {}: не доставлена", chunk.idx + 1),
        });
    }

    let channels = runner.db.channels().await?;
    let broken: Vec<&Channel> = channels.iter().filter(|c| c.last_error.is_some()).collect();
    if !broken.is_empty() {
        out.push_str("\n\nканалы с ошибками:");
        for channel in broken {
            out.push_str(&format!(
                "\n{} — {}",
                channel.label(),
                channel.last_error.as_deref().unwrap_or_default()
            ));
        }
    }
    Ok(out)
}

async fn sync(runner: &Arc<Runner>) -> Result<String> {
    if runner.cfg.telegram.folder.is_none() {
        return Ok("папка не настроена (telegram.folder), каналами управляют /add и /rm".into());
    }
    let Some(report) = runner.sync_folder().await? else {
        return Ok("папку прочитать не удалось, подробности в логе".into());
    };
    if let Some(refused) = report.refused {
        return Ok(refused);
    }
    Ok(format!(
        "в папке каналов: {}\nдобавлено: {}\nвыключено: {}",
        report.total,
        or_dash(&report.added),
        or_dash(&report.disabled)
    ))
}

async fn add(runner: &Arc<Runner>, arg: &str) -> Result<String> {
    if arg.is_empty() {
        return Ok("нужен канал: /add @name, /add https://t.me/+инвайт или /add id".into());
    }

    let peer = resolve_new_channel(runner, arg).await?;
    let (username, title) = folder::describe(&runner.client, peer).await?;

    match runner.cfg.telegram.folder.as_deref() {
        // Единый источник правды — папка: правим её, иначе следующий sync
        // откатит правку.
        Some(name) => match folder::find(&runner.client, name).await? {
            folder::Lookup::Found(f) => {
                folder::add_peer(&runner.client, &f, peer).await?;
                folder::sync(&runner.client, &runner.db, name).await?;
            }
            folder::Lookup::Invalid(reason) => return Ok(reason),
            folder::Lookup::Missing => {
                return Ok(format!("папки {name:?} в аккаунте нет — создайте её"))
            }
        },
        None => {
            runner
                .db
                .upsert_channel(peer.id, peer.access_hash, username.as_deref(), &title)
                .await?;
        }
    }

    Ok(format!("добавлен: {title}, id {}", peer.id))
}

async fn remove(runner: &Arc<Runner>, arg: &str) -> Result<String> {
    if arg.is_empty() {
        return Ok("нужен канал: /rm @name или /rm id".into());
    }
    let Some(channel) = runner.db.find_channel(arg).await? else {
        return Ok(format!("канала {arg} в базе нет, /list покажет список"));
    };

    match runner.cfg.telegram.folder.as_deref() {
        Some(name) => match folder::find(&runner.client, name).await? {
            folder::Lookup::Found(f) => {
                folder::remove_peer(&runner.client, &f, channel.id).await?;
                folder::sync(&runner.client, &runner.db, name).await?;
            }
            folder::Lookup::Invalid(reason) => return Ok(reason),
            folder::Lookup::Missing => return Ok(format!("папки {name:?} в аккаунте нет")),
        },
        None => {
            runner.db.disable_channel(channel.id).await?;
        }
    }

    Ok(format!("выключен: {}, id {}", channel.title, channel.id))
}

async fn reset(runner: &Arc<Runner>, arg: &str) -> Result<String> {
    let Some(channel) = runner.db.find_channel(arg).await? else {
        return Ok(format!("канала {arg} в базе нет, /list покажет список"));
    };
    runner.db.reset_channel(channel.id).await?;
    Ok(format!(
        "{}: курсор сброшен, следующий прогон прочитает окно в {} ч",
        channel.title, runner.cfg.telegram.lookback_hours
    ))
}

/// Осознанный повтор, а не восстановление: части уходят в **текущий**
/// `destination` и с новыми `random_id`.
async fn last(runner: &Arc<Runner>) -> Result<String> {
    let Some(run) = runner.db.last_ok_run().await? else {
        return Ok("опубликованных дайджестов ещё не было".into());
    };
    let chunks: Vec<String> = runner
        .db
        .chunks(run.id)
        .await?
        .into_iter()
        .map(|c| c.html)
        .collect();
    if chunks.is_empty() {
        return Ok(format!("у прогона #{} нет сохранённых частей", run.id));
    }

    let dest =
        publish::resolve_destination(&runner.client, &runner.cfg.telegram.destination).await?;
    publish::resend(
        &runner.client,
        publish::destination_ref(dest)?,
        &chunks,
        runner.cancellation(),
    )
    .await?;

    Ok(format!(
        "прогон #{} отправлен заново в {} ({} частей)",
        run.id,
        runner.cfg.telegram.destination,
        chunks.len()
    ))
}

/// `@username`, инвайт-ссылка или числовой id.
async fn resolve_new_channel(runner: &Arc<Runner>, arg: &str) -> Result<folder::ChannelPeer> {
    let client = &runner.client;

    // Числовой id — особый случай: access_hash взять неоткуда, а без него
    // Telegram на id не отвечает. Смотрим базу, потом диалоги.
    if let Ok(id) = arg.parse::<i64>() {
        let id = crate::db::bare_channel_id(id);
        let known = runner
            .db
            .channel(id)
            .await?
            .map(|channel| folder::ChannelPeer {
                id: channel.id,
                access_hash: channel.access_hash,
            });
        return match known {
            Some(peer) => Ok(peer),
            None => folder::find_in_dialogs(client, id).await?.with_context(|| {
                format!("канала id {id} нет среди диалогов — аккаунт на него подписан?")
            }),
        };
    }

    let peer = if arg.contains("t.me/+") || arg.contains("joinchat") {
        client
            .accept_invite_link(arg)
            .await
            .with_context(|| format!("не удалось присоединиться по ссылке {arg}"))?
            .context("по ссылке не вернулся чат")?
    } else {
        client
            .resolve_username(arg.trim_start_matches('@'))
            .await
            .with_context(|| format!("не удалось разрешить {arg}"))?
            .with_context(|| format!("канал {arg} не найден"))?
    };

    // По username и по инвайту приходит что угодно: пользователь, бот, группа.
    // Дайджест умеет читать только каналы, и `post_url` тоже рассчитан на них.
    if !matches!(peer, Peer::Channel(_)) {
        bail!("{arg} — это {}, а не канал", peer_kind(&peer));
    }

    let reference = peer
        .to_ref()
        .await
        .map_err(|e| anyhow::anyhow!("канал недоступен: {e}"))?
        .context("канал недоступен")?;

    Ok(folder::ChannelPeer {
        id: reference
            .id
            .bare_id()
            .context("у канала нет идентификатора")?,
        access_hash: reference.auth.hash(),
    })
}

fn peer_kind(peer: &Peer) -> &'static str {
    match peer {
        Peer::User(user) if user.is_bot() => "бот",
        Peer::User(_) => "пользователь",
        Peer::Group(_) => "группа",
        Peer::Channel(_) => "канал",
    }
}

fn or_dash(items: &[String]) -> String {
    if items.is_empty() {
        "—".into()
    } else {
        items.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, Destination};
    use grammers_client::{Client, SenderPool};
    use grammers_session::storages::MemorySession;
    use std::collections::BTreeMap;

    async fn runner() -> Arc<Runner> {
        let cfg = toml::from_str(include_str!("../../config.example.toml")).unwrap();
        let db = Arc::new(Db::open(":memory:").await.unwrap());
        // The sender is never run: all commands in these tests are local.
        let pool = SenderPool::new(Arc::new(MemorySession::default()), 1);
        Arc::new(Runner::new(
            Arc::new(cfg),
            db,
            Client::new(pool.handle),
            Cancel::never(),
        ))
    }

    #[tokio::test]
    async fn commands_reject_mutation_but_allow_reads_during_a_run() {
        let runner = runner().await;
        let _guard = runner.try_lock().unwrap();
        for command in [
            "/run",
            "/reset 1",
            "/abandon 1",
            "/resume 1",
            "/sync",
            "/add @news",
            "/rm 1",
            "/last",
        ] {
            let reply = handle(&runner, command).await.unwrap();
            assert!(reply.contains("идёт прогон"), "{command}: {reply}");
        }
        assert!(handle(&runner, "/list").await.is_ok());
        assert!(handle(&runner, "/status").await.is_ok());
        assert_eq!(handle(&runner, "/help").await.unwrap(), HELP);
    }

    #[tokio::test]
    async fn pending_delivery_blocks_reset_until_abandoned() {
        let runner = runner().await;
        runner.db.upsert_channel(1, 7, None, "News").await.unwrap();
        let id = runner.db.start_run("manual").await.unwrap();
        runner
            .db
            .save_summary(
                id,
                1,
                1,
                (None, None),
                &BTreeMap::from([(1, 42)]),
                "me",
                Destination {
                    peer_id: 5,
                    access_hash: 6,
                },
                &[("news".into(), 7)],
            )
            .await
            .unwrap();
        assert!(handle(&runner, "/reset 1")
            .await
            .unwrap()
            .contains("недоставленный"));
        handle(&runner, &format!("/abandon {id}")).await.unwrap();
        assert!(handle(&runner, "/reset 1")
            .await
            .unwrap()
            .contains("курсор сброшен"));
        assert!(runner.db.unfinished_run().await.unwrap().is_none());
        assert_eq!(runner.db.channel(1).await.unwrap().unwrap().last_msg_id, 0);
    }
}
