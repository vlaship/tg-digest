//! Работа с Telegram через MTProto (личный аккаунт).
//!
//! Bot API здесь не подходит: бот читает только те каналы, где он админ.

pub mod client;
pub mod commands;
pub mod folder;
pub mod publish;

use grammers_client::client::Client;
use grammers_client::message::InputMessage;

/// Короткое уведомление владельцу в «Избранное».
///
/// Клиент Telegram уже есть, поэтому это самый дешёвый мониторинг из возможных:
/// иначе про упавший ночной прогон узнаём через неделю из логов. Само
/// уведомление не должно ронять прогон — ошибки только в лог.
pub async fn notify(client: &Client, text: &str) {
    if let Err(e) = try_notify(client, text).await {
        tracing::warn!(error = %e, %text, "не удалось отправить уведомление в Избранное");
    }
}

async fn try_notify(client: &Client, text: &str) -> anyhow::Result<()> {
    let me = client.get_me().await?;
    let peer = me
        .to_ref()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .ok_or_else(|| anyhow::anyhow!("свой профиль недоступен"))?;

    // Текст ошибки бывает длиннее лимита сообщения, а Telegram такое отвергает:
    // уведомление о падении не должно теряться из-за собственного размера.
    for piece in crate::digest::split_plain(text) {
        client
            .send_message(peer, InputMessage::new().text(piece).link_preview(false))
            .await?;
    }
    Ok(())
}
