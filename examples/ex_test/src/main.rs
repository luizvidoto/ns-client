use std::str::FromStr;

use nostr::{secp256k1::XOnlyPublicKey, Filter, Kind, RelayMessage, Timestamp};
use ns_client::{RelayEvent, Subscription};
use tokio::signal;

pub(crate) const RELAY_SUGGESTIONS: [&'static str; 16] = [
    "wss://relay.plebstr.com",
    "wss://relay.snort.social",
    "wss://relay.damus.io",
    "wss://nostr.wine",
    "wss://nostr-pub.wellorder.net",
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
pub(crate) const RELAYS_NIP11: [&'static str; 0] = [
    // "wss://nostr-pub.wellorder.net",
    // "wss://nostr.slothy.win",
    // "wss://relay.stoner.com",
    // "wss://nostr.einundzwanzig.space",
    // "wss://relay.nostr.band",
    // "wss://nostr.mom",
    // "wss://sg.qemura.xyz",
    // "wss://nos.lol",
];
#[tokio::main]
async fn main() {
    env_logger::init();

    let pool = ns_client::RelayPool::new();
    let mut notifications = pool.notifications();
    log::info!("APP: Connecting to pool");

    let mut relay_servers = (15..16)
        .flat_map(|n| {
            (119..120)
                .map(|m| format!("ws://192.168.{}.{}:8080", n, m))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    for relay in RELAYS_NIP11.iter().map(|r| r.to_string()) {
        relay_servers.push(relay);
    }

    for url in relay_servers.iter() {
        match pool.add_relay(url) {
            Ok(url) => {
                log::info!("Added relay: {}", url);
            }
            Err(e) => {
                log::info!("Failed to add relay: {}", e);
            }
        }
    }

    log::info!("APP: Relays: {}", relay_servers.len());

    // let mut terminated_relays: HashSet<String> = HashSet::new();

    let contacts = vec![nostr::key::XOnlyPublicKey::from_str(
        "8860df7d3b24bfb40fe5bdd2041663d35d3e524ce7376628aa55a7d3e624ac46",
    )
    .unwrap()];
    let subscription = Subscription::new(vec![contact_list_metadata_filter(&contacts, 168633199)])
        .with_id("contact_list_metadata");
    pool.subscribe(&subscription).unwrap();

    let pool_h = tokio::spawn(async move {
        while let Ok(event) = notifications.recv().await {
            let url = event.url;
            match event.event {
                RelayEvent::ActionsDone(actions_id) => {
                    log::info!("APP: Actions done: {} - {}", url, actions_id);
                }
                RelayEvent::SentCount(sub_id) => {
                    log::info!("APP: Sent count: {} - {}", url, sub_id);
                }
                RelayEvent::SendError(e) => {
                    log::info!("APP: Send error: {} - {}", url, e);
                }
                RelayEvent::RelayInformation(doc) => {
                    log::info!("APP: Relay document: {} - {:?}", url, doc);
                }
                RelayEvent::Timeout(sub_id) => {
                    log::info!("APP: Timeout: {} - {}", url, sub_id);
                }
                RelayEvent::SentEvent(event_id) => {
                    log::info!("APP: Sent event: {} - {}", url, &event_id);
                }
                RelayEvent::RelayMessage(message) => match message {
                    RelayMessage::Event {
                        subscription_id: sub_id,
                        event,
                    } => {
                        log::info!("APP: Event: {} - {} - {:?}", &url, sub_id, event);
                    }
                    RelayMessage::EndOfStoredEvents(sub_id) => {
                        log::info!("APP: EOSE. ID: {} - {}", &url, sub_id);
                    }
                    other => {
                        log::info!("APP: Relay message: {} - {:?}", url, other);
                    }
                },
                RelayEvent::SentSubscription(sub_id) => {
                    log::info!("APP: Sent subscription: {} - {:?}", url, sub_id);
                }
            }
        }
    });

    // // update 1
    // let contacts = vec![
    //     XOnlyPublicKey::from_str(
    //         "8860df7d3b24bfb40fe5bdd2041663d35d3e524ce7376628aa55a7d3e624ac46",
    //     )
    //     .unwrap(),
    //     XOnlyPublicKey::from_str(
    //         "4cced2fb18ff00d32b4f01a65e9cfcc7e3c607024fd8c6604186de40e50aae03",
    //     )
    //     .unwrap(),
    // ];
    // let subscription = Subscription::new(vec![contact_list_metadata_filter(&contacts, 0)])
    //     .with_id("contact_list_metadata");
    // pool.subscribe(&subscription).unwrap();

    // // update 2
    // let contacts = vec![
    //     XOnlyPublicKey::from_str(
    //         "8860df7d3b24bfb40fe5bdd2041663d35d3e524ce7376628aa55a7d3e624ac46",
    //     )
    //     .unwrap(),
    //     XOnlyPublicKey::from_str(
    //         "4cced2fb18ff00d32b4f01a65e9cfcc7e3c607024fd8c6604186de40e50aae03",
    //     )
    //     .unwrap(),
    //     XOnlyPublicKey::from_str(
    //         "9e45b5e573adfb70be9f81e6f19e3df334fa24b3a7273859104d399ccbf64e94",
    //     )
    //     .unwrap(),
    // ];
    // let subscription = Subscription::new(vec![contact_list_metadata_filter(&contacts, 0)])
    //     .with_id("contact_list_metadata");
    // pool.subscribe(&subscription).unwrap();

    // // update 3
    // let contacts = vec![
    //     XOnlyPublicKey::from_str(
    //         "8860df7d3b24bfb40fe5bdd2041663d35d3e524ce7376628aa55a7d3e624ac46",
    //     )
    //     .unwrap(),
    //     XOnlyPublicKey::from_str(
    //         "4cced2fb18ff00d32b4f01a65e9cfcc7e3c607024fd8c6604186de40e50aae03",
    //     )
    //     .unwrap(),
    //     XOnlyPublicKey::from_str(
    //         "9e45b5e573adfb70be9f81e6f19e3df334fa24b3a7273859104d399ccbf64e94",
    //     )
    //     .unwrap(),
    //     XOnlyPublicKey::from_str(
    //         "dafb7c5a8d3a061a8254eb9ffb132cceec0b5080357531006e127263121e3adc",
    //     )
    //     .unwrap(),
    // ];
    // let subscription = Subscription::new(vec![contact_list_metadata_filter(&contacts, 0)])
    //     .with_id("contact_list_metadata");
    // pool.subscribe(&subscription).unwrap();

    tokio::select! {
        _ = shutdown_signal() => {}
        _ = pool_h => {}
    }
}

pub fn contact_list_metadata_filter(
    contact_list: &[nostr::key::XOnlyPublicKey],
    last_event_tt: u64,
) -> Filter {
    let all_pubkeys = contact_list
        .iter()
        .map(|pubkey| pubkey.to_string())
        .collect::<Vec<_>>();

    Filter::new()
        .authors(all_pubkeys)
        .kind(Kind::Metadata)
        .since(Timestamp::from(last_event_tt))
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
