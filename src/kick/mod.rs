//! Everything that talks to Kick: the official REST API, the website API used for chatroom
//! discovery, the Pusher websocket which carries the chat, and the translation of Kick events
//! into rustlog's message model.

pub mod api;
pub mod convert;
pub mod events;
pub mod pusher;
