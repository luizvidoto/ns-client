mod error;
mod pool;
mod relay;
mod stats;
mod utils;

pub use error::Error;
pub use pool::{NotificationEvent, RelayPool, RelayStatusList};
pub use relay::{
    RelayErrorMessage, RelayEvent, RelayInformation, RelayOptions, RelayState, RelayStatus,
    SendError, Subscription,
};
pub use stats::RelayConnectionStats;

pub type Result<T> = std::result::Result<T, Error>;
