//! Work with Telegram through MTProto (personal account).
//!
//! The Bot API does not fit here: a bot reads only channels where it is an admin.

pub mod channels;
pub mod client;
pub mod commands;
pub mod publish;

use grammers_client::client::Client;
use grammers_client::message::InputMessage;

/// A short notification to the owner in Saved Messages.
///
/// We already have a Telegram client, so this is the cheapest monitoring possible:
/// otherwise we learn about a failed night run from the logs a week later. The
/// notification itself must not break the run — errors go only to the log.
pub async fn notify(client: &Client, text: &str) {
    if let Err(e) = try_notify(client, text).await {
        tracing::warn!(error = %e, %text, "could not send a notification to Saved Messages");
    }
}

async fn try_notify(client: &Client, text: &str) -> anyhow::Result<()> {
    let me = client.get_me().await?;
    let peer = me
        .to_ref()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .ok_or_else(|| anyhow::anyhow!("own profile is not available"))?;

    // An error text can be longer than the message limit, and Telegram rejects that:
    // a failure notification must not get lost because of its own size.
    for piece in crate::digest::split_plain(text) {
        client
            .send_message(peer, InputMessage::new().text(piece).link_preview(false))
            .await?;
    }
    Ok(())
}
