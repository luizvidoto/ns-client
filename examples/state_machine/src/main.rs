use std::{collections::HashMap, time::Instant};

use log::{debug, info};
use nostr::RelayMessage;
use ns_client::{run_pool, PoolInput, PoolState};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    env_logger::init();
    // Create a new instance of Instant, which marks the current point in time.
    let start = Instant::now();

    let (main_tx, mut main_rx) = mpsc::channel(1024);
    let mut pool_tx = None;
    let mut relays = HashMap::new();

    let main_tx_clone = main_tx.clone();
    tokio::spawn(async move {
        let mut pool_state = PoolState::new();
        loop {
            let (event, state) = run_pool(pool_state).await;
            pool_state = state;
            match event {
                ns_client::PoolEvent::None => (),
                other => main_tx_clone.send(other).await.unwrap(),
            }
        }
    });

    while let Some(event) = main_rx.recv().await {
        match event {
            ns_client::PoolEvent::Connected(channel) => {
                debug!("APP: Connected to channel");
                let relays_strs = (119..250)
                    .map(|n| format!("ws://192.168.15.{}:8080", n))
                    .collect::<Vec<_>>();

                for r in relays_strs {
                    channel.try_send(PoolInput::AddRelay(r)).unwrap();
                }

                pool_tx = Some(channel);
            }

            ns_client::PoolEvent::RelayAdded(url) => {
                debug!("APP: Added relay: {}", url);
                relays.insert(url.to_owned(), RelayStatus::Disconnected);
            }
            ns_client::PoolEvent::FailedToAddRelay { url, message } => {
                debug!("APP: Failed to add relay: {} - {}", url, message)
            }
            ns_client::PoolEvent::None => (),

            ns_client::PoolEvent::RelayConnected(url) => {
                debug!("APP: Relay connected");
                relays.insert(url.to_owned(), RelayStatus::Connected);
                if let Some(ch) = &mut pool_tx {
                    let meta_filter = nostr::Filter::new().kinds(vec![nostr::Kind::Metadata]);
                    ch.try_send(PoolInput::SubscribeTo((url, vec![meta_filter])))
                        .unwrap();
                }
            }
            ns_client::PoolEvent::RelayConnecting(url) => {
                debug!("APP: relay connecting: {}", url);
                relays.insert(url.to_owned(), RelayStatus::Connecting);
            }
            ns_client::PoolEvent::RelayDisconnected(url) => {
                debug!("APP: relay disconnected: {}", url);
                relays.insert(url.to_owned(), RelayStatus::Disconnected);
            }
            ns_client::PoolEvent::RelayMessage((url, message)) => {
                match message {
                    RelayMessage::EndOfStoredEvents(sub_id) => {
                        debug!("APP: End of stored events: {}", sub_id);
                        // Get the time elapsed since the creation of the Instant.
                        let duration = start.elapsed();

                        // Display the elapsed time.
                        info!("Time elapsed in expensive_function() is: {:?}", duration);
                        return;
                    }
                    other => {
                        debug!("APP: Relay message: {} - {:?}", url, other)
                    }
                }
            }
            ns_client::PoolEvent::RelayError((url, message)) => {
                debug!("APP: Relay error: {} - {:?}", url, message)
            }
            ns_client::PoolEvent::SentSubscription((url, sub_id)) => {
                debug!("APP: Sent subscription: {} - {:?}", url, sub_id)
            } // ns_client::PoolEvent::None => (),
        }

        // for (url, status) in relays.iter() {
        //     debug!("APP: {}: {}", url, status)
        // }
        // debug!("                               ");
    }
}

enum RelayStatus {
    Connected,
    Connecting,
    Disconnected,
}
impl std::fmt::Display for RelayStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelayStatus::Connected => write!(f, "Connected"),
            RelayStatus::Connecting => write!(f, "Connecting"),
            RelayStatus::Disconnected => write!(f, "Disconnected"),
        }
    }
}
