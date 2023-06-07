use log::debug;
use nostr::RelayMessage;
use ns_client::RelayEvent;

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
pub(crate) const RELAYS_NIP11: [&'static str; 8] = [
    "wss://nostr-pub.wellorder.net",
    "wss://nostr.slothy.win",
    "wss://relay.stoner.com",
    "wss://nostr.einundzwanzig.space",
    "wss://relay.nostr.band",
    "wss://nostr.mom",
    "wss://sg.qemura.xyz",
    "wss://nos.lol",
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
                debug!("Added relay: {}", url);
            }
            Err(e) => {
                debug!("Failed to add relay: {}", e);
            }
        }
    }

    log::info!("APP: Relays: {}", relay_servers.len());

    // let mut terminated_relays: HashSet<String> = HashSet::new();

    while let Ok(event) = notifications.recv().await {
        let url = event.url;
        match event.event {
            RelayEvent::ActionsDone(actions_id) => {
                log::debug!("APP: Actions done: {} - {}", url, actions_id);
            }
            RelayEvent::SentCount(sub_id) => {
                log::debug!("APP: Sent count: {} - {}", url, sub_id);
            }
            RelayEvent::SendError(e) => {
                log::debug!("APP: Send error: {} - {}", url, e);
            }
            RelayEvent::RelayInformation(doc) => {
                log::info!("APP: Relay document: {} - {:?}", url, doc);
            }
            RelayEvent::Timeout(sub_id) => {
                log::debug!("APP: Timeout: {} - {}", url, sub_id);
            }
            RelayEvent::SentEvent(event_id) => {
                log::debug!("APP: Sent event: {} - {}", url, &event_id);
            }
            RelayEvent::RelayMessage(message) => match message {
                RelayMessage::Event {
                    subscription_id: sub_id,
                    event: _,
                } => {
                    log::debug!("APP: Event: {} - {}", &url, sub_id);
                }
                RelayMessage::EndOfStoredEvents(sub_id) => {
                    log::info!("APP: EOSE. ID: {} - {}", &url, sub_id);
                }
                other => {
                    log::debug!("APP: Relay message: {} - {:?}", url, other);
                }
            },
            RelayEvent::SentSubscription(sub_id) => {
                log::debug!("APP: Sent subscription: {} - {:?}", url, sub_id);
            }
        }
    }

    log::info!("APP: Done");
}

// async fn _main() {
//     env_logger::init();

//     let keys = Keys::generate();
//     let client = nostr_sdk::Client::new(&keys);
//     let mut notifications = client.notifications();
//     log::info!("APP: Connecting to pool");

//     let mut relay_servers = (15..16)
//         .flat_map(|n| {
//             (119..120)
//                 .map(|m| format!("ws://192.168.{}.{}:8080", n, m))
//                 .collect::<Vec<_>>()
//         })
//         .collect::<Vec<_>>();

//     for relay in RELAYS_NIP11.iter().map(|r| r.to_string()) {
//         relay_servers.push(relay);
//     }

//     for url in relay_servers.iter() {
//         match client.add_relay(url.clone(), None).await {
//             Ok(_) => {
//                 debug!("Added relay: {}", url);
//             }
//             Err(e) => {
//                 debug!("Failed to add relay: {}", e);
//             }
//         }
//     }

//     log::info!("APP: Relays: {}", relay_servers.len());

//     tokio::spawn(async move {
//         while let Ok(event) = notifications.recv().await {
//             match event {
//                 RelayPoolNotification::Event(url, _event) => {
//                     log::info!("APP: Event: {}", &url);
//                 }
//                 RelayPoolNotification::Shutdown => {
//                     log::info!("APP: Shutdown");
//                 }
//                 RelayPoolNotification::Message(url, message) => match message {
//                     RelayMessage::EndOfStoredEvents(sub_id) => {
//                         log::info!("APP: EOSE. ID: {} - {}", &url, sub_id);
//                     }
//                     other => {
//                         log::info!("APP: Relay message: {} - {:?}", url, other);
//                     }
//                 },
//             }
//         }
//     });

//     let mut interval = tokio::time::interval(Duration::from_secs(5));
//     for _ in 0..10 {
//         interval.tick().await;
//         for (url, relay) in client.relays().await {
//             let doc = relay.document().await;
//             if let Some(name) = doc.name {
//                 log::info!("Relay {} - {:?}", url, name);
//             }
//         }
//     }

//     log::info!("APP: Done");
// }
