use std::{collections::HashSet, path::Path, sync::Arc, time::Duration};

use log::debug;
use nostr::{EventId, Filter, RelayMessage, SubscriptionId, Url};
use ns_client::{RelayEvent, RelayPool, SendError, Subscription};
use tokio::{
    fs::OpenOptions,
    io::{AsyncWriteExt, BufWriter},
    signal,
    sync::{mpsc, Mutex},
};

pub(crate) const RELAY_SUGGESTIONS: [&'static str; 3] = [
    "wss://relay.plebstr.com",
    "wss://relay.snort.social",
    "wss://relay.damus.io",
    // "wss://nostr.wine",
    // "wss://nostr-pub.wellorder.net",
    // "wss://nostr1.tunnelsats.com",
    // "wss://relay.nostr.info",
    // "wss://nostr-relay.wlvs.space",
    // "wss://nostr.zebedee.cloud",
    // "wss://lbrygen.xyz",
    // "wss://nostr.8e23.net",
    // "wss://nostr.xmr.rocks",
    // "wss://xmr.usenostr.org",
    // "wss://relay.roli.social",
    // "wss://relay.nostr.ro",
    // "wss://nostr.swiss-enigma.ch",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubscriptionType {
    ContactList,
    ContactListMetadata,
    Messages,
    Channel,
    ChannelMetadata,
}
impl std::fmt::Display for SubscriptionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubscriptionType::ContactList => write!(f, "ContactList"),
            SubscriptionType::ContactListMetadata => write!(f, "ContactListMetadata"),
            SubscriptionType::Messages => write!(f, "Messages"),
            SubscriptionType::Channel => write!(f, "Channel"),
            SubscriptionType::ChannelMetadata => write!(f, "ChannelMetadata"),
        }
    }
}

#[tokio::main]
async fn main() {
    env_logger::init();
    // Create a new instance of Instant, which marks the current point in time.
    // let start = Instant::now();

    let pool = RelayPool::new();
    let mut notifications = pool.notifications();
    log::info!("APP: Connecting to pool");

    // let meta_filter = Filter::new().kinds(vec![nostr::Kind::Metadata]);
    // let sent_msgs_sub_past = Filter::new()
    //     .kinds(nostr_kinds())
    //     .author(public_key.to_string());
    // let recv_msgs_sub_past = Filter::new().kinds(nostr_kinds()).pubkey(public_key);
    // let src_channel_id_1 =
    //     EventId::from_str("f23e652ffda1871d71cd5e1fd6a1f7cc6ef3b42415551814c5a025b68f5bf174")
    //         .unwrap();
    // let src_channel_id_2 =
    //     EventId::from_str("5d7807b7476b78bbb53f9c97aa90b45e44a56f0a709460320e1b6c1a5b16a364")
    //         .unwrap();

    // let cars_channel =
    //     EventId::from_hex("8233a5d8e27a9415d22c974d70935011664ada55ae3152bd10d697d3a3c74f67")
    //         .unwrap();

    let creation_filter = Filter::new().kind(nostr::Kind::ChannelCreation).limit(10);
    let channel_sub_type = SubscriptionId::new(SubscriptionType::Channel.to_string());
    if let Err(e) = pool.subscribe_eose(
        &channel_sub_type,
        vec![creation_filter],
        Some(Duration::from_secs(5)),
    ) {
        log::error!("Failed to subscribe: {}", e);
    }

    // let contact_list_id = SubscriptionId::new(SubscriptionType::ContactList.to_string());
    // let contact_list = Filter::new()
    //     .kind(nostr::Kind::ContactList)
    //     .limit(CHANNEL_SEARCH_LIMIT);
    // if let Err(e) = pool.subscribe_eose(&contact_list_id, vec![contact_list]) {
    //     log::error!("Failed to subscribe to contact_list: {}", e);
    // }

    let mut relay_servers = (15..16)
        .flat_map(|n| {
            (119..120)
                .map(|m| format!("ws://192.168.{}.{}:8080", n, m))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    for relay in RELAY_SUGGESTIONS.iter().map(|r| r.to_string()) {
        relay_servers.push(relay);
    }

    for url in relay_servers.iter() {
        match pool.add_relay(url) {
            Ok(url) => {
                debug!("Added relay: {}", url);
            }
            Err(e) => {
                debug!("Failed to add relay: {}", e);
            }
        }
    }

    log::info!("APP: Relays: {}", relay_servers.len());

    // let mut terminated_relays: HashSet<String> = HashSet::new();

    let (tx, rx) = mpsc::channel(100);
    let file_path = Path::new("output.json");

    let tx_1 = tx.clone();
    let pool_h = tokio::spawn(async move {
        let mut acc = 0;
        let channel_ids = Arc::new(Mutex::new(HashSet::new()));
        while let Ok(notification) = notifications.recv().await {
            let url = notification.url;
            match notification.event {
                RelayEvent::ActionsDone(action_id) => {
                    log::info!("APP: Actions done: {} - {}", url, action_id);
                    acc += 1;
                }
                RelayEvent::SentCount(sub_id) => {
                    log::info!("APP: Sent count: {} - {}", url, sub_id);
                }
                RelayEvent::SendError(e) => match e {
                    SendError::FailedToCloseSubscription(sub_id) => {
                        log::info!("APP: Failed to close subscription: {} - {}", url, sub_id);
                    }
                    SendError::FailedToSendCount(sub_id) => {
                        log::info!("APP: Failed to send count: {} - {}", url, sub_id);
                    }
                    SendError::FailedToSendEvent(id) => {
                        log::info!("APP: Failed to send event: {} - {}", url, id);
                    }
                    SendError::FailedToSendSubscription(sub_id) => {
                        log::info!("APP: Failed to send subscription: {} - {}", url, sub_id);
                    }
                },
                RelayEvent::RelayInformation(doc) => {
                    log::info!("APP: Relay document: {} - {:?}", url, doc);
                }
                RelayEvent::Timeout(subscription_id) => {
                    log::info!("APP: Timeout: {} - {}", url, subscription_id);
                    if subscription_id == channel_sub_type {
                        send_actions(&pool, &url, &channel_ids).await;
                    }
                }
                RelayEvent::SentEvent(event_id) => {
                    log::info!("APP: Sent event: {} - {}", url, &event_id);
                }
                RelayEvent::RelayMessage(message) => match message {
                    RelayMessage::Event {
                        subscription_id,
                        event,
                    } => {
                        if subscription_id == channel_sub_type {
                            let mut locked = channel_ids.lock().await;
                            locked.insert(event.id.to_owned());
                        }
                        if let Err(e) = tx_1.send(event.as_json()).await {
                            log::error!("An error occurred while sending the event: {}", e);
                        }
                    }
                    RelayMessage::EndOfStoredEvents(subscription_id) => {
                        log::info!(
                            "APP: END OF STORED EVENTS. ID: {} - {}",
                            &url,
                            subscription_id
                        );
                        if subscription_id == channel_sub_type {
                            send_actions(&pool, &url, &channel_ids).await;
                        }
                    }
                    other => {
                        debug!("APP: Relay message: {} - {:?}", url, other);
                    }
                },
                RelayEvent::SentSubscription(sub_id) => {
                    log::info!("APP: Sent subscription: {} - {:?}", url, sub_id);
                }
            }

            if acc == 3 {
                break;
            };
        }
    });

    let writer_h = tokio::spawn(async move {
        _ = tokio::fs::remove_file(file_path).await;
        if let Err(e) = receive_and_write(rx, &file_path).await {
            log::error!("An error occurred while writing to the file: {}", e);
        }
    });

    let tx = tx.clone();
    tokio::select! {
        _ = shutdown_signal() => {}
        _ = writer_h => {}
        _ = pool_h => {}
    }

    log::info!("APP: Shutdown signal received");
    if let Err(e) = tx.send("END".to_string()).await {
        log::error!("An error occurred while sending the event: {}", e);
    }

    log::info!("APP: Done");
}

pub fn make_channel_filter(channel_id: EventId) -> Vec<Filter> {
    let metadata_filter = Filter::new()
        .kind(nostr::Kind::ChannelMetadata)
        .event(channel_id)
        .limit(CHANNEL_SEARCH_LIMIT);
    let messages_filter = Filter::new()
        .kind(nostr::Kind::ChannelMessage)
        .event(channel_id)
        .limit(CHANNEL_SEARCH_LIMIT);
    let hide_msgs_filter = Filter::new()
        .kind(nostr::Kind::ChannelHideMessage)
        .event(channel_id)
        .limit(CHANNEL_SEARCH_LIMIT);
    let mute_filter = Filter::new()
        .kind(nostr::Kind::ChannelMuteUser)
        .event(channel_id)
        .limit(CHANNEL_SEARCH_LIMIT);
    vec![
        metadata_filter,
        messages_filter,
        hide_msgs_filter,
        mute_filter,
    ]
}

pub fn nostr_kinds() -> Vec<nostr::Kind> {
    [
        nostr::Kind::Metadata,
        nostr::Kind::EncryptedDirectMessage,
        nostr::Kind::RecommendRelay,
        nostr::Kind::ContactList,
    ]
    .to_vec()
}
async fn receive_and_write(
    mut receiver: mpsc::Receiver<String>,
    file_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(file_path)
        .await?;

    log::info!("APP: Writing to file: {:?}", file_path);

    let mut writer = BufWriter::new(file);

    let mut first = true;

    while let Some(data) = receiver.recv().await {
        if data == "END" {
            // Write the closing bracket and break the loop
            writer.write_all(b"]").await?;
            break;
        }

        let json = if first {
            first = false;
            format!("[\n{}\n", data)
        } else {
            format!(",{}\n", data)
        };

        if let Err(e) = writer.write_all(json.as_bytes()).await {
            log::error!("An error occurred while writing to the file: {}", e);
            break;
        }
    }

    writer.flush().await?;

    Ok(())
}

async fn send_actions(pool: &RelayPool, url: &Url, channel_ids: &Arc<Mutex<HashSet<EventId>>>) {
    let locked = channel_ids.lock().await;
    let actions_id = "SomeActionID123".into();
    _ = pool.relay_eose_actions(
        &url,
        actions_id,
        locked
            .clone()
            .into_iter()
            .map(make_channel_filter)
            .map(|filters| Subscription::action(filters).timeout(Some(Duration::from_secs(2))))
            .collect(),
    );
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    println!("signal received, starting graceful shutdown");
}

const CHANNEL_SEARCH_LIMIT: usize = 1000;
