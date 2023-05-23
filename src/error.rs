use thiserror::Error;
use tokio_tungstenite::tungstenite::Error as WebSocketError;

use crate::ToMain;

#[derive(Error, Debug)]
pub enum Error {
    #[error("WebSocket error: {0}")]
    WebSocketError(#[from] WebSocketError),

    #[error("Nostr error: {0}")]
    NostrError(#[from] NostrError),

    #[error("URL parsing error: {0}")]
    UrlParseError(#[from] url::ParseError),

    #[error("Failed to send to: {0}")]
    NoWebsocketConnection(String),

    #[error("Failed to send to main pool channel: {0:?}")]
    FailedToSendToMain(ToMain),

    #[error("Failed to send to a channel: {0}")]
    SendError(String),
}

#[derive(Error, Debug)]
pub enum NostrError {
    #[error("Event error: {0}")]
    NostrEventError(#[from] nostr::event::Error),
    #[error("Key error: {0}")]
    NostrKeyError(#[from] nostr::key::Error),
}