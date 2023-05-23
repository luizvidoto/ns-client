use std::time::Instant;

use log::{debug, info};

#[tokio::main]
async fn main() -> Result<(), ns_client::Error> {
    env_logger::init();
    // Create a new instance of Instant, which marks the current point in time.
    let start = Instant::now();

    let relays_strs = (119..250)
        .map(|n| format!("ws://192.168.15.{}:8080", n))
        .collect::<Vec<_>>();

    let mut pool = ns_client::RelayPool::new();
    for url in relays_strs {
        pool.add_relay(&url)?;
    }

    let mut notifications = pool.notifications();

    let meta_filter = nostr::Filter::new().kinds(vec![nostr::Kind::Metadata]);
    pool.subscribe(vec![meta_filter]).await;

    while let Ok(notification) = notifications.recv().await {
        if let ns_client::RelayPoolNotification::RelayMessage { url, message } = notification {
            match message {
                nostr::RelayMessage::Event {
                    subscription_id,
                    event,
                } => {
                    debug!("SUBSCRIPTION ID: {}", subscription_id);
                    debug!("{}: {:?}", url, event);
                }
                nostr::RelayMessage::Notice { message } => {
                    debug!("Notice: {}", message);
                }
                nostr::RelayMessage::EndOfStoredEvents(sub_id) => {
                    debug!("END OF STORED EVENTS. ID: {}", sub_id);
                    // Get the time elapsed since the creation of the Instant.
                    let duration = start.elapsed();
                    // Display the elapsed time.
                    info!("Time elapsed in expensive_function() is: {:?}", duration);
                    return Ok(());
                }
                nostr::RelayMessage::Ok {
                    event_id,
                    status,
                    message,
                } => {
                    debug!("OK: {} {} {}", event_id, status, message);
                }
                nostr::RelayMessage::Count {
                    subscription_id,
                    count,
                } => {
                    debug!("COUNT: {} {}", subscription_id, count);
                }
                nostr::RelayMessage::Auth { challenge } => {
                    debug!("CHALLENGE: {}", challenge);
                }
                nostr::RelayMessage::Empty => {}
            }
        }
    }

    Ok(())
}
