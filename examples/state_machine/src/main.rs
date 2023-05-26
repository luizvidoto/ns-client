use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    time::{Duration, Instant},
};

use log::debug;
use nostr::RelayMessage;

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
async fn main() {
    env_logger::init();
    // Create a new instance of Instant, which marks the current point in time.
    let start = Instant::now();

    let mut events: HashMap<nostr::Url, (nostr::SubscriptionId, Vec<nostr::Event>)> =
        HashMap::new();

    let secret_key = nostr::secp256k1::SecretKey::from_str(ACC_1).expect("Invalid secret key");
    let keys = nostr::Keys::new(secret_key);
    let public_key = keys.public_key();

    let pool = ns_client::RelayPool::new();
    let mut notifications = pool.notifications();
    println!("APP: Connecting to pool");

    let meta_filter = nostr::Filter::new().kinds(vec![nostr::Kind::Metadata]);
    let sent_msgs_sub_past = nostr::Filter::new()
        .kinds(nostr_kinds())
        .author(public_key.to_string());
    let recv_msgs_sub_past = nostr::Filter::new().kinds(nostr_kinds()).pubkey(public_key);
    if let Err(e) = pool.subscribe(vec![meta_filter, sent_msgs_sub_past, recv_msgs_sub_past]) {
        log::error!("Failed to subscribe to metadata: {}", e);
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

    let pool_1 = pool.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(status) = pool_1.relay_status_list().await {
                for (url, status) in status.iter() {
                    println!("APP: {}: {}", url, status)
                }
            }
        }
    });

    while let Ok(event) = notifications.recv().await {
        match event {
            ns_client::NotificationEvent::SentEvent((url, event_id)) => {
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
            ns_client::NotificationEvent::RelayMessage((url, message)) => {
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
                        // break;
                    }
                    other => {
                        debug!("APP: Relay message: {} - {:?}", url, other);
                    }
                }
            }
            ns_client::NotificationEvent::SentSubscription((url, sub_id)) => {
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
