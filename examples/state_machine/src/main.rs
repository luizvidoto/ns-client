use std::{collections::HashSet, path::Path};

use log::debug;
use nostr::{RelayMessage, SubscriptionId};
use tokio::{
    fs::OpenOptions,
    io::{AsyncWriteExt, BufWriter},
    sync::mpsc,
};

pub(crate) const RELAY_SUGGESTIONS: [&'static str; 0] = [
    // "wss://relay.plebstr.com",
    // "wss://nostr.wine",
    // "wss://relay.snort.social",
    // "wss://nostr-pub.wellorder.net",
    // "wss://relay.damus.io",
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

    let pool = ns_client::RelayPool::new();
    let mut notifications = pool.notifications();
    println!("APP: Connecting to pool");

    // let meta_filter = nostr::Filter::new().kinds(vec![nostr::Kind::Metadata]);
    // let sent_msgs_sub_past = nostr::Filter::new()
    //     .kinds(nostr_kinds())
    //     .author(public_key.to_string());
    // let recv_msgs_sub_past = nostr::Filter::new().kinds(nostr_kinds()).pubkey(public_key);
    let channel_creation = nostr::Filter::new()
        .kind(nostr::Kind::ChannelCreation)
        .limit(CHANNEL_SEARCH_LIMIT);
    let channel_id = SubscriptionId::new(SubscriptionType::Channel.to_string());
    if let Err(e) = pool.subscribe_eose(&channel_id, vec![channel_creation]) {
        log::error!("Failed to subscribe to metadata: {}", e);
    }

    let contact_list_id = SubscriptionId::new(SubscriptionType::ContactList.to_string());
    let contact_list = nostr::Filter::new()
        .kind(nostr::Kind::ContactList)
        .limit(CHANNEL_SEARCH_LIMIT);
    if let Err(e) = pool.subscribe_eose(&contact_list_id, vec![contact_list]) {
        log::error!("Failed to subscribe to contact_list: {}", e);
    }

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

    println!("APP: Relays: {}", relay_servers.len());

    let mut terminated_relays: HashSet<String> = HashSet::new();

    let (tx, rx) = mpsc::channel(100);
    let file_path = Path::new("output.json");
    tokio::spawn(async move {
        if let Err(e) = receive_and_write(rx, &file_path).await {
            eprintln!("An error occurred while writing to the file: {}", e);
        }
    });

    while let Ok(event) = notifications.recv().await {
        match event {
            ns_client::NotificationEvent::SentEvent(url, event_id) => {
                debug!("APP: Sent event: {} - {}", url, &event_id);
            }
            ns_client::NotificationEvent::RelayTerminated(url) => {
                debug!("APP: relay terminated: {}", &url);
                terminated_relays.insert(url.to_string());
                RELAY_SUGGESTIONS.iter().for_each(|r| {
                    if terminated_relays.contains(&r.to_string()) {
                        println!("APP: Terminated relays: {}", terminated_relays.len());
                        println!("APP: relay terminated: {}", &r);
                    }
                })
            }
            ns_client::NotificationEvent::RelayMessage(url, message) => {
                match message {
                    RelayMessage::Event {
                        subscription_id: _,
                        event,
                    } => {
                        // if let Some((_sub_id, sub)) = events.get_mut(&url) {
                        //     sub.push(*event);
                        // } else {
                        //     events.insert(url, (subscription_id, vec![*event]));
                        // }
                        if let Err(e) = tx.send(*event).await {
                            eprintln!("An error occurred while sending the event: {}", e);
                        }
                    }
                    RelayMessage::EndOfStoredEvents(sub_id) => {
                        println!("APP: END OF STORED EVENTS. ID: {} - {}", &url, sub_id);
                        if sub_id.to_string() == SubscriptionType::Channel.to_string() {
                            let id =
                                SubscriptionId::new(SubscriptionType::ChannelMetadata.to_string());
                            let channel_metadata = nostr::Filter::new()
                                .kind(nostr::Kind::ChannelMetadata)
                                .limit(CHANNEL_SEARCH_LIMIT);
                            if let Err(e) =
                                pool.relay_subscribe_eose(&url, &id, vec![channel_metadata])
                            {
                                log::error!("Failed to subscribe to metadata: {}", e);
                            }
                        }

                        if sub_id.to_string() == SubscriptionType::ContactList.to_string() {
                            let id = SubscriptionId::new(
                                SubscriptionType::ContactListMetadata.to_string(),
                            );
                            let channel_metadata = nostr::Filter::new()
                                .kind(nostr::Kind::Metadata)
                                .limit(CHANNEL_SEARCH_LIMIT);
                            if let Err(e) =
                                pool.relay_subscribe_eose(&url, &id, vec![channel_metadata])
                            {
                                log::error!("Failed to subscribe to metadata: {}", e);
                            }
                        }
                    }
                    other => {
                        debug!("APP: Relay message: {} - {:?}", url, other);
                    }
                }
            }
            ns_client::NotificationEvent::SentSubscription(url, sub_id) => {
                debug!("APP: Sent subscription: {} - {:?}", url, sub_id);
            } // ns_client::PoolEvent::None => (),
        }
    }

    println!("APP: Done");
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

// async fn receive_and_write(
//     mut receiver: mpsc::Receiver<nostr::Event>,
//     file_path: &Path,
// ) -> Result<(), Box<dyn std::error::Error>> {
//     let mut file = tokio::fs::File::create(file_path).await?;

//     while let Some(data) = receiver.recv().await {
//         let json = serde_json::to_string(&data)?;
//         file.write_all(json.as_bytes()).await?;
//     }

//     Ok(())
// }

async fn receive_and_write(
    mut receiver: mpsc::Receiver<nostr::Event>,
    file_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(file_path)
        .await?;

    let mut writer = BufWriter::new(file);

    let mut first = true;

    while let Some(data) = receiver.recv().await {
        let json = if first {
            first = false;
            format!("[\n{}\n", serde_json::to_string(&data)?)
        } else {
            format!(",{}\n", serde_json::to_string(&data)?)
        };

        writer.write_all(json.as_bytes()).await?;
    }

    writer.write_all(b"]").await?;

    Ok(())
}

const CHANNEL_SEARCH_LIMIT: usize = 100;
