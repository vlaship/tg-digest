//! Work with Telegram through MTProto.
//!
//! Two clients: the personal account reads the channels (a bot reads only channels
//! where it is an admin), and the bot talks to the owner and posts the digests.

pub mod bot;
pub mod channels;
pub mod client;
pub mod commands;
pub mod photos;
pub mod publish;
