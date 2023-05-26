use futures_util::stream::SplitSink;
use futures_util::stream::SplitStream;
use futures_util::SinkExt;
use futures_util::StreamExt;
use nostr::ClientMessage;
use nostr::Filter;
use nostr::RelayMessage;
use nostr::SubscriptionId;
use std::collections::VecDeque;
use std::time::Duration;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::error::Error as WsError;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use url::Url;

use crate::pool::RelayToPool;
use crate::utils::spawn_get_connection;
use crate::utils::ConnResult;
use crate::NotificationEvent;
use crate::RelayConnectionStats;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Failed to send to websocket: {0}")]
    FailedToSendToSocket(String),
}

pub async fn spawn_relay_task(
    url: Url,
    pool_tx: mpsc::Sender<RelayToPool>,
    relay_input_sender: mpsc::Sender<RelayInput>,
    relay_input_receiver: mpsc::Receiver<RelayInput>,
    notification_sender: broadcast::Sender<NotificationEvent>,
) {
    let mut relay = Relay::new(&url, relay_input_receiver);
    let mut state = RelayState::new();
    loop {
        let (event_opt, new_state) = run_relay(&mut relay, state).await;
        state = new_state;
        if let Some(event) = event_opt {
            match event {
                RelayEvent::RelayToPool(to_pool_event) => {
                    if let Err(_e) = pool_tx
                        .send(RelayToPool {
                            url: url.clone(),
                            event: to_pool_event,
                        })
                        .await
                    {
                        log::debug!("CLOSED - {}", url);
                        if let Err(e) = relay_input_sender.try_send(RelayInput::Close) {
                            log::debug!("Failed to send close to relay: {}", e);
                        }
                        break;
                    }
                }
                RelayEvent::RelayNotification(notification_event) => {
                    if let Some(event) = notification_event.to_notification(&url) {
                        if let Err(e) = notification_sender.send(event) {
                            log::debug!("Failed to send to notification receiver: {}", e)
                        }
                    }
                }
            }
        }
    }
}

pub struct Relay {
    url: Url,
    stats: RelayConnectionStats,
    relay_input_receiver: mpsc::Receiver<RelayInput>,
}
impl Relay {
    fn new(url: &Url, relay_input_receiver: mpsc::Receiver<RelayInput>) -> Self {
        Self {
            relay_input_receiver,
            stats: RelayConnectionStats::new(),
            url: url.to_owned(),
        }
    }
}

pub enum RelayState {
    Connected {
        ws_read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        ws_write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
        input_queue: VecDeque<RelayInput>,
    },
    Connecting {
        one_rx: mpsc::Receiver<ConnResult>,
        input_queue: VecDeque<RelayInput>,
        delay_to_connect_millis: u64,
    },
    Disconnected {
        delay_to_connect_millis: u64,
        input_queue: VecDeque<RelayInput>,
    },
    Terminated,
}

impl std::fmt::Display for RelayState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelayState::Connected { .. } => write!(f, "Connected"),
            RelayState::Connecting { .. } => write!(f, "Connecting"),
            RelayState::Disconnected { .. } => write!(f, "Disconnected"),
            RelayState::Terminated { .. } => write!(f, "Terminated"),
        }
    }
}
impl RelayState {
    pub fn is_connected(&self) -> bool {
        match self {
            Self::Connected { .. } => true,
            _ => false,
        }
    }
    pub fn get_status(&self) -> RelayStatus {
        match self {
            RelayState::Connected { .. } => RelayStatus::Connected,
            RelayState::Connecting { .. } => RelayStatus::Connecting,
            RelayState::Disconnected { .. } => RelayStatus::Disconnected,
            RelayState::Terminated { .. } => RelayStatus::Terminated,
        }
    }
    pub fn new() -> Self {
        let input_queue = VecDeque::new();
        RelayState::Disconnected {
            delay_to_connect_millis: 0,
            input_queue,
        }
    }
    pub fn retry_with_delay(
        delay_to_connect_millis: u64,
        input_queue: VecDeque<RelayInput>,
    ) -> Self {
        let delay_to_connect_millis = if delay_to_connect_millis == 0 {
            1000
        } else {
            delay_to_connect_millis * 2
        };
        RelayState::Disconnected {
            delay_to_connect_millis,
            input_queue,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub enum RelayStatus {
    #[default]
    Disconnected,
    Connected,
    Connecting,
    Terminated,
}
impl RelayStatus {
    pub fn is_connected(&self) -> bool {
        match self {
            Self::Connected { .. } => true,
            _ => false,
        }
    }
}
impl std::fmt::Display for RelayStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelayStatus::Connected { .. } => write!(f, "Connected"),
            RelayStatus::Connecting { .. } => write!(f, "Connecting"),
            RelayStatus::Disconnected { .. } => write!(f, "Disconnected"),
            RelayStatus::Terminated { .. } => write!(f, "Terminated"),
        }
    }
}

fn handle_ws_message(url: &Url, message: Option<Result<Message, WsError>>) -> Option<RelayEvent> {
    match message {
        Some(Ok(msg)) if msg.is_text() => {
            // Handle incoming messages here
            match msg.to_text() {
                Ok(msg_text) => match RelayMessage::from_json(msg_text) {
                    Ok(message) => Some(RelayEvent::RelayNotification(
                        RelayNotificationEvent::RelayMessage(message),
                    )),
                    Err(e) => {
                        log::debug!("Error parsing message from {}: {}", &url, e);
                        None
                    }
                },
                Err(e) => {
                    log::debug!("Error parsing message from {}: {}", &url, e);
                    None
                }
            }
        }
        Some(Ok(msg)) if msg.is_close() => {
            Some(RelayEvent::RelayToPool(RelayToPoolEvent::RelayDisconnected))
        }
        Some(Err(e)) => {
            log::debug!("Error reading message from {}: {}", &url, e);
            None
        }
        _ => None,
    }
}

async fn handle_relay_input(
    ws_write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    input: RelayInput,
) -> Result<Option<RelayEvent>, Error> {
    match input {
        RelayInput::SendEvent(event) => {
            log::debug!("Sending event");
            let event_id = event.id;
            let msg = ClientMessage::new_event(event);
            send_ws(ws_write, msg).await?;
            log::debug!("Sent event");
            Ok(Some(RelayEvent::RelayNotification(
                RelayNotificationEvent::SentEvent(event_id),
            )))
        }
        RelayInput::Subscribe(sub_id, filters) => {
            log::debug!("Sending subscription request");
            let msg = ClientMessage::new_req(sub_id.clone(), filters);
            send_ws(ws_write, msg).await?;
            Ok(Some(RelayEvent::RelayNotification(
                RelayNotificationEvent::SentSubscription(sub_id),
            )))
        }
        RelayInput::Count(subscription_id, filters) => {
            log::debug!("Sending count request");
            let msg = ClientMessage::new_count(subscription_id, filters);
            send_ws(ws_write, msg).await?;
            log::debug!("Sent count request");
            Ok(None)
        }
        RelayInput::Close => {
            log::debug!("Closing relay");
            let _ = ws_write.close().await;
            Ok(Some(RelayEvent::RelayToPool(RelayToPoolEvent::Closed)))
        }
        RelayInput::Reconnect => {
            log::debug!("Ignoring reconnect request");
            Ok(None)
        }
    }
}

async fn send_ws(
    ws_write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    msg: ClientMessage,
) -> Result<(), Error> {
    ws_write
        .send(Message::text(msg.as_json()))
        .await
        .map_err(|e| Error::FailedToSendToSocket(e.to_string()))?;
    Ok(())
}

#[derive(Debug, Clone)]
pub enum RelayInput {
    SendEvent(nostr::Event),
    Subscribe(SubscriptionId, Vec<Filter>),
    Close,
    Reconnect,
    Count(SubscriptionId, Vec<Filter>),
}
#[derive(Debug, Clone)]
pub enum RelayEvent {
    RelayToPool(RelayToPoolEvent),
    RelayNotification(RelayNotificationEvent),
}
#[derive(Debug, Clone)]
pub enum RelayToPoolEvent {
    Reconnecting,
    FailedToConnect,
    ConnectedToSocket,
    RelayDisconnected,
    AttempToConnect,
    Closed,
    MaxAttempsExceeded,
    TerminateRelay,
}
#[derive(Debug, Clone)]
pub enum RelayNotificationEvent {
    RelayMessage(RelayMessage),
    SentSubscription(SubscriptionId),
    SentEvent(nostr::EventId),
}
impl RelayNotificationEvent {
    pub fn to_notification(self: RelayNotificationEvent, url: &Url) -> Option<NotificationEvent> {
        match self {
            RelayNotificationEvent::RelayMessage(message) => {
                Some(NotificationEvent::RelayMessage(url.to_owned(), message))
            }
            RelayNotificationEvent::SentSubscription(sub_id) => {
                Some(NotificationEvent::SentSubscription(url.to_owned(), sub_id))
            }
            RelayNotificationEvent::SentEvent(event_id) => {
                Some(NotificationEvent::SentEvent(url.to_owned(), event_id))
            }
        }
    }
}

pub async fn run_relay(relay: &mut Relay, state: RelayState) -> (Option<RelayEvent>, RelayState) {
    match state {
        RelayState::Terminated => {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(RECONNECT_DELAY_SECS)) => {
                        (None, RelayState::Terminated)
                }
                input = relay.relay_input_receiver.recv() => {
                    if let Some(input) = input {
                        if let RelayInput::Reconnect = input {
                            return (Some(
                                RelayEvent::RelayToPool(
                                    RelayToPoolEvent::Reconnecting
                                ))
                            , RelayState::new())
                        } else {
                            log::debug!("{} - other input - {:?}", &relay.url, input);
                        }
                    } else {
                        log::debug!("{} - relay input receiver closed", &relay.url);
                    }
                    (None, RelayState::Terminated)
                }
            }
        }
        RelayState::Disconnected {
            delay_to_connect_millis,
            input_queue,
        } => {
            relay.stats.new_attempt();
            let attempts = relay.stats.attempts();
            if attempts >= MAX_ATTEMPS {
                log::trace!("{} - max attempts exceeded - {}", &relay.url, attempts);
                return (
                    Some(RelayEvent::RelayToPool(
                        RelayToPoolEvent::MaxAttempsExceeded,
                    )),
                    RelayState::Terminated,
                );
            }

            tokio::time::sleep(Duration::from_millis(delay_to_connect_millis)).await;

            let (one_tx, one_rx) = mpsc::channel(1);
            spawn_get_connection(relay.url.clone(), one_tx);

            (
                Some(RelayEvent::RelayToPool(RelayToPoolEvent::AttempToConnect)),
                RelayState::Connecting {
                    one_rx,
                    input_queue,
                    delay_to_connect_millis,
                },
            )
        }
        RelayState::Connecting {
            mut one_rx,
            mut input_queue,
            delay_to_connect_millis,
        } => {
            let event_opt = tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(GET_CONN_DELAY_MILLIS)) => {
                        log::trace!("{} - waiting", &relay.url);
                        None
                }
                input = relay.relay_input_receiver.recv() => {
                    if let Some(input) = input {
                        input_queue.push_front(input);
                        log::debug!("{} - input q: {}", &relay.url, input_queue.len());
                    }
                    None
                }
                received = one_rx.recv() => {
                    match received {
                        Some(Ok(ws_stream)) => {
                            log::debug!("{} - CONN OK", &relay.url);
                            let (ws_write, ws_read) = ws_stream.split();
                            return (
                                Some(RelayEvent::RelayToPool(RelayToPoolEvent::ConnectedToSocket)),
                                RelayState::Connected {
                                    ws_read,
                                    ws_write,
                                    input_queue
                                },
                            );
                        }
                        Some(Err(e)) => {
                            if let Some(tungstenite_error) = e.downcast_ref::<tungstenite::Error>() {
                                match tungstenite_error {
                                    tungstenite::Error::Io(_) => {
                                        log::trace!("{} - retry with delay", &relay.url);
                                        return (
                                            Some(RelayEvent::RelayToPool(RelayToPoolEvent::FailedToConnect)),
                                            RelayState::retry_with_delay(delay_to_connect_millis, input_queue)
                                        )
                                    },
                                    other => log::info!("{} - tungstenite error: {}", &relay.url, other),
                                }
                            }
                            log::info!("{} - conn err: {}", &relay.url, e);
                        }
                        None => {
                            log::debug!("{} - one_rx closed", &relay.url);
                        }
                    }
                    return (
                        Some(RelayEvent::RelayToPool(RelayToPoolEvent::TerminateRelay)),
                        RelayState::Terminated,
                    );
                }
            };
            (
                event_opt,
                RelayState::Connecting {
                    one_rx,
                    input_queue,
                    delay_to_connect_millis,
                },
            )
        }
        RelayState::Connected {
            mut ws_read,
            mut ws_write,
            mut input_queue,
        } => {
            relay.stats.new_success();
            let event_opt = if let Some(input) = input_queue.pop_front() {
                match handle_relay_input(&mut ws_write, input).await {
                    Ok(event_opt) => event_opt,
                    Err(e) => {
                        log::debug!("{}: {}", &relay.url, e);
                        None
                    }
                }
            } else {
                // Receive subscription response
                let event = tokio::select! {
                    message = ws_read.next() => {
                        handle_ws_message(&relay.url, message)
                    }
                    input = relay.relay_input_receiver.recv() => {
                        if let Some(input) = input {
                            match handle_relay_input(&mut ws_write, input).await {
                                Ok(event_opt) => event_opt,
                                Err(e) => {
                                    log::debug!("{}: {}", &relay.url, e);
                                    None
                                }
                            }
                        } else {
                            log::debug!("{}: Relay input channel closed", &relay.url);
                            None
                        }
                    }
                };
                event
            };

            if let Some(RelayEvent::RelayToPool(RelayToPoolEvent::Closed)) = event_opt {
                return (
                    Some(RelayEvent::RelayToPool(RelayToPoolEvent::Closed)),
                    RelayState::Terminated,
                );
            }

            (
                event_opt,
                RelayState::Connected {
                    ws_read,
                    ws_write,
                    input_queue,
                },
            )
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RelayOptions {
    pub read: bool,
    pub write: bool,
}
impl Default for RelayOptions {
    fn default() -> Self {
        Self {
            read: true,
            write: true,
        }
    }
}
impl RelayOptions {
    pub fn new(read: bool, write: bool) -> Self {
        Self { read, write }
    }
}

const GET_CONN_DELAY_MILLIS: u64 = 100;
const RECONNECT_DELAY_SECS: u64 = 2;
const MAX_ATTEMPS: usize = 3;
