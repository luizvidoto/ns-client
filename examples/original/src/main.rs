use std::{collections::HashMap, str::FromStr, time::Instant};

use log::debug;
use nostr_sdk::{Keys, Options, RelayMessage, RelayPoolNotification};

const ACC_1: &'static str = "4510459b74db68371be462f19ef4f7ef1e6c5a95b1d83a7adf00987c51ac56fe";

pub(crate) const RELAY_SUGGESTIONS: [&'static str; 16] = [
    "wss://relay.plebstr.com",
    "wss://nostr.wine",
    "wss://relay.snort.social",
    "wss://nostr-pub.wellorder.net",
    "wss://relay.damus.io",
    "wss://nostr1.tunnelsats.com",
    "wss://relay.nostr.info",
    "wss://nostr-relay.wlvs.space",
    "wss://nostr.zebedee.cloud",
    "wss://lbrygen.xyz",
    "wss://nostr.8e23.net",
    "wss://nostr.xmr.rocks",
    "wss://xmr.usenostr.org",
    "wss://relay.roli.social",
    "wss://relay.nostr.ro",
    "wss://nostr.swiss-enigma.ch",
];

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // env_logger::init();
    // Create a new instance of Instant, which marks the current point in time.
    let start = Instant::now();

    let mut events: HashMap<nostr_sdk::Url, (nostr_sdk::SubscriptionId, Vec<nostr_sdk::Event>)> =
        HashMap::new();

    let secret_key = nostr_sdk::secp256k1::SecretKey::from_str(ACC_1).expect("Invalid secret key");
    let keys = Keys::new(secret_key);
    let public_key = keys.public_key();

    let client = nostr_sdk::Client::with_opts(
        &keys,
        Options::new()
            .wait_for_send(false)
            .wait_for_connection(false)
            .wait_for_subscription(false),
    );

    let mut relay_servers = (100..150)
        .flat_map(|n| {
            (10..20)
                .map(|m| format!("ws://192.168.{}.{}:8080", m, n))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    for relay in RELAY_SUGGESTIONS.iter().map(|r| r.to_string()) {
        relay_servers.push(relay);
    }
    println!("APP: Relays: {}", relay_servers.len());

    for url in relay_servers.iter() {
        client.add_relay(url, None).await?;
    }

    client.connect().await;

    let mut notifications = client.notifications();

    let meta_filter = nostr_sdk::Filter::new().kinds(vec![nostr_sdk::Kind::Metadata]);
    let sent_msgs_sub_past = nostr_sdk::Filter::new()
        .kinds(nostr_kinds())
        .author(public_key.to_string());
    let recv_msgs_sub_past = nostr_sdk::Filter::new()
        .kinds(nostr_kinds())
        .pubkey(public_key);
    client
        .subscribe(vec![meta_filter, sent_msgs_sub_past, recv_msgs_sub_past])
        .await;

    while let Ok(notification) = notifications.recv().await {
        if let RelayPoolNotification::Message(url, message) = notification {
            match message {
                RelayMessage::Event {
                    subscription_id,
                    event,
                } => {
                    if let Some((_sub_id, sub)) = events.get_mut(&url) {
                        sub.push(*event);
                    } else {
                        events.insert(url, (subscription_id, vec![*event]));
                    }
                }
                RelayMessage::Notice { message } => {
                    debug!("APP: Notice: {}", message);
                }
                RelayMessage::EndOfStoredEvents(sub_id) => {
                    println!("APP: END OF STORED EVENTS. ID: {}", sub_id);
                    for (url, (sub, events)) in events.iter() {
                        println!("APP: events. {} - ID: {} - {}", &url, sub, events.len());
                    }

                    // Get the time elapsed since the creation of the Instant.
                    // let duration = start.elapsed();
                    // Display the elapsed time.
                    // println!(
                    //     "APP: Time elapsed in expensive_function() is: {:?}",
                    //     duration
                    // );
                    // return Ok(());
                }
                RelayMessage::Ok {
                    event_id,
                    status,
                    message,
                } => {
                    debug!("APP: OK: {} {} {}", event_id, status, message);
                }
                RelayMessage::Count {
                    subscription_id,
                    count,
                } => {
                    debug!("APP: COUNT: {} {}", subscription_id, count);
                }
                RelayMessage::Auth { challenge } => {
                    debug!("APP: CHALLENGE: {}", challenge);
                }
                RelayMessage::Empty => {}
            }
        }
    }

    Ok(())
}

pub fn nostr_kinds() -> Vec<nostr_sdk::Kind> {
    [
        nostr_sdk::Kind::Metadata,
        nostr_sdk::Kind::EncryptedDirectMessage,
        nostr_sdk::Kind::RecommendRelay,
        nostr_sdk::Kind::ContactList,
    ]
    .to_vec()
}
