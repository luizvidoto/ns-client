use futures_util::stream::SplitSink;
use futures_util::stream::SplitStream;
use futures_util::SinkExt;
use futures_util::StreamExt;
use nostr::ClientMessage;
use nostr::Filter;
use nostr::RelayMessage;
use nostr::SubscriptionId;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use url::Url;

use crate::get_conn;
use crate::ConnResult;
use crate::Error;
use crate::RelayConnectionStats;

#[derive(Debug, Clone)]
pub enum PoolInput {
    AddRelay(String),
    SubscribeTo((Url, Vec<Filter>)),
}
#[derive(Debug, Clone)]
pub enum PoolEvent {
    Connected(mpsc::Sender<PoolInput>),
    RelayAdded(Url),
    RelayConnected(Url),
    RelayConnecting(Url),
    RelayDisconnected(Url),
    RelayMessage((Url, RelayMessage)),
    RelayError((Url, String)),
    SentSubscription((Url, SubscriptionId)),
    FailedToAddRelay { url: String, message: String },
    None,
}

pub enum PoolState {
    Disconnected,
    Connected {
        main_rx: mpsc::Receiver<PoolInput>,
        inner_tx: mpsc::Sender<RelayToPool>,
        inner_rx: mpsc::Receiver<RelayToPool>,
        relays: HashMap<Url, Option<mpsc::Sender<RelayInput>>>,
    },
}
impl PoolState {
    pub fn new() -> Self {
        PoolState::Disconnected
    }
}
pub async fn run_pool(state: PoolState) -> (PoolEvent, PoolState) {
    match state {
        PoolState::Disconnected => {
            let (main_tx, main_rx) = mpsc::channel(CHANNEL_SIZE);
            let (inner_tx, inner_rx) = mpsc::channel(CHANNEL_SIZE);
            let relays = HashMap::new();
            (
                PoolEvent::Connected(main_tx),
                PoolState::Connected {
                    main_rx,
                    relays,
                    inner_tx,
                    inner_rx,
                },
            )
        }
        PoolState::Connected {
            mut main_rx,
            mut relays,
            inner_tx,
            mut inner_rx,
        } => {
            let event = tokio::select! {
                inner = inner_rx.recv() => {
                    if let Some(msg) = inner {
                        match msg {
                            RelayToPool::Processed {url, event} => {
                                process_relay_event(&url, event, &mut relays)
                            }
                            // _ => PoolEvent::None
                        }
                    } else {
                        PoolEvent::None
                    }
                }
                input = main_rx.recv() => {
                    if let Some(input) = input {
                        match input {
                            PoolInput::AddRelay(url) => {
                                process_add_relay(&inner_tx, url, &mut relays).await
                            }
                            PoolInput::SubscribeTo((url, filters)) => {
                                process_subscribe_to(url, filters, &mut relays).await
                            }
                        }
                    } else {
                        PoolEvent::None
                    }
                }

            };

            (
                event,
                PoolState::Connected {
                    main_rx,
                    relays,
                    inner_rx,
                    inner_tx,
                },
            )
        }
    }
}

fn process_relay_event(
    url: &Url,
    event: RelayEvent,
    relays: &mut HashMap<Url, Option<mpsc::Sender<RelayInput>>>,
) -> PoolEvent {
    match event {
        RelayEvent::StoredInput => PoolEvent::None,
        RelayEvent::WebsocketClosed => PoolEvent::RelayDisconnected(url.to_owned()),
        RelayEvent::FailedToConnect => PoolEvent::None,
        RelayEvent::ChannelConnected(conn) => {
            relays.entry(url.to_owned()).and_modify(|tx| {
                *tx = Some(conn);
            });
            PoolEvent::None
        }
        RelayEvent::SentSubscription(sub_id) => {
            PoolEvent::SentSubscription((url.to_owned(), sub_id))
        }
        RelayEvent::ConnectedToSocket => PoolEvent::RelayConnected(url.to_owned()),
        RelayEvent::WaitingForSocket => PoolEvent::RelayConnecting(url.to_owned()),
        RelayEvent::DisconnectedFromSocket => PoolEvent::RelayDisconnected(url.to_owned()),
        RelayEvent::RelayMessage(message) => PoolEvent::RelayMessage((url.to_owned(), message)),
        RelayEvent::Error(message) => PoolEvent::RelayError((url.to_owned(), message)),
        RelayEvent::ChannelDisconnected => {
            // retry connection?
            PoolEvent::None
        }
        RelayEvent::None => PoolEvent::None,
    }
}

pub enum RelayState {
    Connected {
        url: Url,
        ws_read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        ws_write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
        stats: RelayConnectionStats,
        inner_rx: mpsc::Receiver<RelayInput>,
        input_queue: VecDeque<RelayInput>,
    },
    Connecting {
        url: Url,
        stats: RelayConnectionStats,
        one_rx: mpsc::Receiver<ConnResult>,
        inner_rx: mpsc::Receiver<RelayInput>,
        input_queue: VecDeque<RelayInput>,
    },
    Disconnected {
        url: Url,
        stats: RelayConnectionStats,
    },
}
impl RelayState {
    pub fn new(url: &Url) -> Self {
        RelayState::Disconnected {
            url: url.to_owned(),
            stats: RelayConnectionStats::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum RelayInput {
    None,
    Subscribe(Vec<Filter>),
}
#[derive(Debug, Clone)]
pub enum RelayEvent {
    ChannelConnected(mpsc::Sender<RelayInput>),
    SentSubscription(SubscriptionId),
    ConnectedToSocket,
    WaitingForSocket,
    DisconnectedFromSocket,
    FailedToConnect,
    RelayMessage(RelayMessage),
    Error(String),
    ChannelDisconnected,
    StoredInput,
    WebsocketClosed,
    None,
}

pub enum RelayToPool {
    // None,
    // Connected(Url),
    // Error {
    //     url: Url,
    //     message: String,
    // },
    Processed { url: Url, event: RelayEvent },
}

pub async fn run_relay(state: RelayState) -> (RelayEvent, RelayState) {
    match state {
        RelayState::Disconnected { url, stats } => {
            let (one_tx, one_rx) = mpsc::channel(1);
            let (inner_tx, inner_rx) = mpsc::channel(1024);
            get_conn(&url, one_tx);
            let input_queue = VecDeque::new();
            (
                RelayEvent::ChannelConnected(inner_tx),
                RelayState::Connecting {
                    url,
                    stats,
                    inner_rx,
                    one_rx,
                    input_queue,
                },
            )
        }
        RelayState::Connecting {
            url,
            stats,
            mut one_rx,
            mut inner_rx,
            mut input_queue,
        } => {
            let event = tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(DELAY_MILLIS)) => {
                        RelayEvent::WaitingForSocket
                }
                input = inner_rx.recv() => {
                    if let Some(input) = input {
                        input_queue.push_front(input);
                        RelayEvent::StoredInput
                    } else {
                        RelayEvent::None
                    }
                }
                received = one_rx.recv() => {
                    if let Some(conn) = received {
                        match conn {
                            ConnResult::Ok(ws_stream) => {
                                log::info!("CONN OK");
                                let (ws_write, ws_read) = ws_stream.split();
                                return (
                                    RelayEvent::ConnectedToSocket,
                                    RelayState::Connected {
                                        url,
                                        ws_read,
                                        ws_write,
                                        stats,
                                        inner_rx,
                                        input_queue
                                    },
                                );
                            }
                            ConnResult::Error(e) => {
                                log::error!("Error connecting to {}: {}", &url, e);
                                return (
                                    RelayEvent::Error(e.to_string()),
                                    RelayState::Disconnected { url, stats },
                                );
                            }
                        }
                    } else {
                        return (
                            RelayEvent::FailedToConnect,
                            RelayState::Disconnected { url, stats },
                        );
                    }
                }
            };
            (
                event,
                RelayState::Connecting {
                    url,
                    stats,
                    one_rx,
                    inner_rx,
                    input_queue,
                },
            )
        }
        RelayState::Connected {
            url,
            mut ws_read,
            mut ws_write,
            mut inner_rx,
            stats,
            mut input_queue,
        } => {
            let event = if let Some(input) = input_queue.pop_front() {
                match handle_relay_input(&mut ws_write, input).await {
                    Ok(event) => event,
                    Err(e) => RelayEvent::Error(e.to_string()),
                }
            } else {
                // Receive subscription response
                let event = tokio::select! {
                    message = ws_read.next() => {
                        match message {
                            Some(Ok(msg)) if msg.is_text() => {
                                // Handle incoming messages here
                                match msg.to_text() {
                                    Ok(msg_text) => {
                                        match RelayMessage::from_json(msg_text) {
                                            Ok(message) => RelayEvent::RelayMessage(message),
                                            Err(e) => RelayEvent::Error(e.to_string()),
                                        }
                                    }
                                    Err(e) => {
                                        RelayEvent::Error(e.to_string())
                                    }
                                }
                            }
                            Some(Ok(msg)) if msg.is_close() => {
                                RelayEvent::DisconnectedFromSocket
                            }
                            Some(Err(e)) => {
                                RelayEvent::Error(e.to_string())
                            }
                            _ => RelayEvent::None,
                        }
                    }
                    input = inner_rx.recv() => {
                        if let Some(input) = input {
                            match handle_relay_input(&mut ws_write, input).await {
                                Ok(event) => event,
                                Err(e) => {
                                    RelayEvent::Error(e.to_string())
                                }
                            }
                        } else {
                            RelayEvent::None
                        }
                    }
                };
                event
            };

            if let RelayEvent::WebsocketClosed = event {
                return (
                    RelayEvent::WebsocketClosed,
                    RelayState::Disconnected { url, stats },
                );
            } else {
                (
                    event,
                    RelayState::Connected {
                        stats,
                        url,
                        ws_read,
                        ws_write,
                        inner_rx,
                        input_queue,
                    },
                )
            }
        }
    }
}

async fn handle_relay_input(
    ws_write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    input: RelayInput,
) -> Result<RelayEvent, Error> {
    match input {
        RelayInput::None => Ok(RelayEvent::None),
        RelayInput::Subscribe(filters) => {
            let sub_id = SubscriptionId::new("abc");
            let msg = ClientMessage::new_req(sub_id.clone(), filters);
            ws_write.send(Message::text(msg.as_json())).await?;
            log::info!("Sent subscription request");
            Ok(RelayEvent::SentSubscription(sub_id))
        }
    }
}

async fn process_add_relay(
    inner_tx: &mpsc::Sender<RelayToPool>,
    url: String,
    relays: &mut HashMap<Url, Option<mpsc::Sender<RelayInput>>>,
) -> PoolEvent {
    match Url::parse(&url) {
        Ok(url) => {
            let relay_task = spawn_relay(url.clone(), inner_tx.clone());
            tokio::spawn(relay_task);
            relays.insert(url.clone(), None::<mpsc::Sender<RelayInput>>);
            PoolEvent::RelayAdded(url)
        }
        Err(e) => PoolEvent::FailedToAddRelay {
            url: url.to_owned(),
            message: e.to_string(),
        },
    }
}

async fn process_subscribe_to(
    url: Url,
    filters: Vec<Filter>,
    relays: &mut HashMap<Url, Option<mpsc::Sender<RelayInput>>>,
) -> PoolEvent {
    if let Some(relay_tx) = relays.get_mut(&url).and_then(Option::as_mut) {
        if let Err(e) = relay_tx.try_send(RelayInput::Subscribe(filters)) {
            log::error!("Failed to send subscription to relay: {}", e);
        }
    } else {
        log::warn!("Relay not found or not connected");
    }
    PoolEvent::None
}

async fn spawn_relay(url: Url, inner_tx: mpsc::Sender<RelayToPool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = RelayState::new(&url);
        loop {
            let (event, new_state) = run_relay(state).await;
            state = new_state;
            if let Err(e) = inner_tx
                .send(RelayToPool::Processed {
                    url: url.clone(),
                    event,
                })
                .await
            {
                log::error!("Failed to send to pool: {}", e);
            }
        }
    })
}

const CHANNEL_SIZE: usize = 1024;
const DELAY_MILLIS: u64 = 100;
