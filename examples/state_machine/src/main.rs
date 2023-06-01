use std::{collections::HashSet, path::Path, str::FromStr, time::Duration};

use log::debug;
use nostr::{
    nips::nip21::NostrURI,
    prelude::{FromBech32, FromPkStr, FromSkStr},
    ChannelId, EventId, Keys, RelayMessage, SubscriptionId, Tag, Url,
};
use tokio::{
    fs::OpenOptions,
    io::{AsyncWriteExt, BufWriter},
    sync::mpsc,
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

    let pool = ns_client::RelayPool::new();
    let mut notifications = pool.notifications();
    log::info!("APP: Connecting to pool");

    // let meta_filter = nostr::Filter::new().kinds(vec![nostr::Kind::Metadata]);
    // let sent_msgs_sub_past = nostr::Filter::new()
    //     .kinds(nostr_kinds())
    //     .author(public_key.to_string());
    // let recv_msgs_sub_past = nostr::Filter::new().kinds(nostr_kinds()).pubkey(public_key);
    // let src_channel_id_1 =
    //     EventId::from_str("f23e652ffda1871d71cd5e1fd6a1f7cc6ef3b42415551814c5a025b68f5bf174")
    //         .unwrap();
    // let src_channel_id_2 =
    //     EventId::from_str("5d7807b7476b78bbb53f9c97aa90b45e44a56f0a709460320e1b6c1a5b16a364")
    //         .unwrap();

    let cars_channel =
        EventId::from_hex("8233a5d8e27a9415d22c974d70935011664ada55ae3152bd10d697d3a3c74f67")
            .unwrap();
    let creation_filter = nostr::Filter::new()
        .kind(nostr::Kind::ChannelCreation)
        // .id(cars_channel)
        .limit(CHANNEL_SEARCH_LIMIT);
    let metadata_filter = nostr::Filter::new()
        .kind(nostr::Kind::ChannelMetadata)
        // .event(cars_channel)
        .limit(CHANNEL_SEARCH_LIMIT);
    let messages_filter = nostr::Filter::new()
        .kind(nostr::Kind::ChannelMessage)
        // .event(cars_channel)
        .limit(CHANNEL_SEARCH_LIMIT);
    let hide_msgs_filter = nostr::Filter::new()
        .kind(nostr::Kind::ChannelHideMessage)
        // .event(cars_channel)
        .limit(CHANNEL_SEARCH_LIMIT);
    let mute_filter = nostr::Filter::new()
        .kind(nostr::Kind::ChannelMuteUser)
        // .event(cars_channel)
        .limit(CHANNEL_SEARCH_LIMIT);

    if let Err(e) = pool.subscribe_eose(
        &SubscriptionId::new(SubscriptionType::Channel.to_string()),
        vec![
            creation_filter,
            metadata_filter,
            messages_filter,
            hide_msgs_filter,
            mute_filter,
        ],
        Some(Duration::from_secs(3)),
    ) {
        log::error!("Failed to subscribe: {}", e);
    }

    // let contact_list_id = SubscriptionId::new(SubscriptionType::ContactList.to_string());
    // let contact_list = nostr::Filter::new()
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
    tokio::spawn(async move {
        let mut acc = 0;
        while let Ok(event) = notifications.recv().await {
            match event {
                ns_client::NotificationEvent::Timeout(url, sub_id) => {
                    log::info!("APP: Timeout: {} - {}", url, sub_id);
                    acc += 1;
                }
                ns_client::NotificationEvent::SentEvent(url, event_id) => {
                    log::info!("APP: Sent event: {} - {}", url, &event_id);
                }
                ns_client::NotificationEvent::RelayTerminated(url) => {
                    log::info!("APP: relay terminated: {}", &url);
                }
                ns_client::NotificationEvent::RelayMessage(url, message) => match message {
                    RelayMessage::Event {
                        subscription_id: _,
                        event,
                    } => {
                        if let Err(e) = tx.send(event.as_json()).await {
                            log::error!("An error occurred while sending the event: {}", e);
                        }
                    }
                    RelayMessage::EndOfStoredEvents(sub_id) => {
                        log::info!("APP: END OF STORED EVENTS. ID: {} - {}", &url, sub_id);
                        acc += 1;
                    }
                    other => {
                        debug!("APP: Relay message: {} - {:?}", url, other);
                    }
                },
                ns_client::NotificationEvent::SentSubscription(url, sub_id) => {
                    log::info!("APP: Sent subscription: {} - {:?}", url, sub_id);
                }
            }

            if acc == 3 {
                if let Err(e) = tx.send("END".to_string()).await {
                    log::error!("An error occurred while sending the event: {}", e);
                }
            };
        }
    });

    // tokio::spawn(async move {
    // });
    tokio::fs::remove_file(file_path).await.unwrap();
    if let Err(e) = receive_and_write(rx, &file_path).await {
        log::error!("An error occurred while writing to the file: {}", e);
    }

    // let cars_channel =
    //     ChannelId::from_hex("8233a5d8e27a9415d22c974d70935011664ada55ae3152bd10d697d3a3c74f67")
    //         .unwrap();
    // let keys =
    //     Keys::from_sk_str("4510459b74db68371be462f19ef4f7ef1e6c5a95b1d83a7adf00987c51ac56fe")
    //         .unwrap();
    // let my_relay = Url::parse("ws://192.168.15.119:8080").unwrap();
    // let event = nostr::EventBuilder::new_channel_msg(cars_channel, my_relay, "eai amigos")
    //     .to_event(&keys)
    //     .unwrap();
    // pool.send_event(event).unwrap();

    // tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    log::info!("APP: Done");
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

const CHANNEL_SEARCH_LIMIT: usize = 1000;
