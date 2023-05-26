use thiserror::Error;

use crate::pool::PoolInput;

#[derive(Error, Debug)]
pub enum Error {
    #[error("URL parsing error: {0}")]
    UrlParseError(#[from] url::ParseError),

    #[error("Failed to send to pool task: {0} - PoolInput: {1:?}")]
    SendToPoolTaskFailed(String, PoolInput),

    #[error("Failed to get relays status")]
    UnableToGetRelaysStatus,

    #[error("{0}")]
    FromRelayError(#[from] crate::relay::Error),
}
