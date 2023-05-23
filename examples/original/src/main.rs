use std::time::Instant;

use log::{debug, info};
use nostr_sdk::{Keys, RelayMessage, RelayPoolNotification};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    env_logger::init();
    // Create a new instance of Instant, which marks the current point in time.
    let start = Instant::now();

    let relays_strs = (119..250)
        .map(|n| format!("ws://192.168.15.{}:8080", n))
        .collect::<Vec<_>>();

    let keys = Keys::generate();
    let client = nostr_sdk::Client::new(&keys);

    for url in relays_strs {
        client.add_relay(&url, None).await?;
    }

    client.connect().await;

    let mut notifications = client.notifications();

    let meta_filter = nostr_sdk::Filter::new().kinds(vec![nostr_sdk::Kind::Metadata]);
    client.subscribe(vec![meta_filter]).await;

    while let Ok(notification) = notifications.recv().await {
        if let RelayPoolNotification::Message(url, message) = notification {
            match message {
                RelayMessage::Event {
                    subscription_id,
                    event,
                } => {
                    debug!("SUBSCRIPTION ID: {}", subscription_id);
                    debug!("{}: {:?}", url, event);
                }
                RelayMessage::Notice { message } => {
                    debug!("Notice: {}", message);
                }
                RelayMessage::EndOfStoredEvents(sub_id) => {
                    debug!("END OF STORED EVENTS. ID: {}", sub_id);
                    // Get the time elapsed since the creation of the Instant.
                    let duration = start.elapsed();
                    // Display the elapsed time.
                    info!("Time elapsed in expensive_function() is: {:?}", duration);
                    return Ok(());
                }
                RelayMessage::Ok {
                    event_id,
                    status,
                    message,
                } => {
                    debug!("OK: {} {} {}", event_id, status, message);
                }
                RelayMessage::Count {
                    subscription_id,
                    count,
                } => {
                    debug!("COUNT: {} {}", subscription_id, count);
                }
                RelayMessage::Auth { challenge } => {
                    debug!("CHALLENGE: {}", challenge);
                }
                RelayMessage::Empty => {}
            }
        }
    }

    Ok(())
}
