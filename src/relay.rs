use futures_util::future::pending;
use futures_util::future::Either;
use futures_util::stream::SplitSink;
use futures_util::stream::SplitStream;
use futures_util::SinkExt;
use futures_util::StreamExt;
use nostr::ClientMessage;
use nostr::Filter;
use nostr::RelayMessage;
use nostr::SubscriptionId;
use std::collections::HashMap;
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

    #[error("Can't write to relay: {0}")]
    BlockedWriteToRelay(Url),

    #[error("Can't read from relay: {0}")]
    BlockedReadFromRelay(Url),
}

pub async fn spawn_relay_task(
    url: Url,
    opts: RelayOptions,
    pool_tx: mpsc::Sender<RelayToPool>,
    relay_input_sender: mpsc::Sender<RelayInput>,
    relay_input_receiver: mpsc::Receiver<RelayInput>,
    notification_sender: broadcast::Sender<NotificationEvent>,
) {
    let mut relay = Relay::new(&url, relay_input_receiver, opts);
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
                        log::debug!("CLOSED - {}", &url);
                        if let Err(e) = relay_input_sender.try_send(RelayInput::Close) {
                            log::debug!("Failed to send close to relay: {}", e);
                        }
                        break;
                    }
                }
                RelayEvent::RelayNotification(notification_event) => {
                    let event = notification_event.to_event(&url);

                    check_subscription_eose(&event, &relay, &relay_input_sender);
                    check_timeout(&event, &relay, &relay_input_sender);

                    if notification_sender.send(event).is_err() {
                        log::debug!("Failed to send event to notification receiver.");
                    }
                }
            }
        }
    }
}

fn check_subscription_eose(
    event: &NotificationEvent,
    relay: &Relay,
    relay_input_sender: &mpsc::Sender<RelayInput>,
) {
    if let NotificationEvent::RelayMessage(_, RelayMessage::EndOfStoredEvents(sub_id)) = event {
        if let Some(subscription) = relay.subscriptions.get(sub_id) {
            if let SubscriptionType::UntilEOSE = subscription.sub_type {
                if relay_input_sender
                    .try_send(RelayInput::CloseSubscription(sub_id.to_owned()))
                    .is_err()
                {
                    log::debug!("Failed to send CloseSubscription signal to relay.");
                }
            }
        }
    }
}

fn check_timeout(
    event: &NotificationEvent,
    relay: &Relay,
    relay_input_sender: &mpsc::Sender<RelayInput>,
) {
    if let NotificationEvent::Timeout(_, sub_id) = event {
        if let Some(subscription) = relay.subscriptions.get(sub_id) {
            if relay_input_sender
                .try_send(RelayInput::CloseSubscription(sub_id.to_owned()))
                .is_err()
            {
                log::debug!("Failed to send CloseSubscription signal to relay.");
            }
        }
    }
}

pub struct Relay {
    url: Url,
    stats: RelayConnectionStats,
    read: bool,
    write: bool,
    relay_input_receiver: mpsc::Receiver<RelayInput>,
    subscriptions: HashMap<SubscriptionId, Subscription>,
    timeout_tx: mpsc::Sender<SubscriptionId>,
    timeout_rx: mpsc::Receiver<SubscriptionId>,
}
impl Relay {
    fn new(
        url: &Url,
        relay_input_receiver: mpsc::Receiver<RelayInput>,
        opts: RelayOptions,
    ) -> Self {
        let (timeout_tx, timeout_rx) = mpsc::channel(10);
        Self {
            relay_input_receiver,
            stats: RelayConnectionStats::new(),
            url: url.to_owned(),
            subscriptions: HashMap::new(),
            read: opts.read,
            write: opts.write,
            timeout_rx,
            timeout_tx,
        }
    }
    async fn handle_input(
        &mut self,
        ws_write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
        input: RelayInput,
    ) -> Result<Option<RelayEvent>, Error> {
        match input {
            RelayInput::ToggleRead(read) => {
                self.read = read;
                Ok(None)
            }
            RelayInput::ToggleWrite(write) => {
                self.write = write;
                Ok(None)
            }
            RelayInput::CloseSubscription(sub_id) => {
                log::debug!("Sending close subscription");
                let msg = ClientMessage::close(sub_id.clone());
                log::debug!("{}", msg.as_json());
                self.send_ws(ws_write, msg).await?;
                log::debug!("Sent close subscription");

                self.subscriptions.remove(&sub_id);

                Ok(None)
            }
            RelayInput::SendEvent(event) => {
                log::debug!("Sending event");
                let event_id = event.id;
                let msg = ClientMessage::new_event(event);
                log::debug!("{}", msg.as_json());
                self.send_ws(ws_write, msg).await?;
                log::debug!("Sent event");
                Ok(Some(RelayEvent::RelayNotification(
                    RelayNotificationEvent::SentEvent(event_id),
                )))
            }
            RelayInput::Subscribe(subscription) => {
                log::debug!("Sending subscription request");
                if self.subscriptions.contains_key(&subscription.id) {
                    log::info!("Already sent subscription");
                    return Ok(None);
                }

                let sub_id = subscription.id.clone();
                let timeout_sender = self.timeout_tx.clone();
                if let Some(timeout) = subscription.timeout {
                    tokio::spawn(async move {
                        tokio::time::sleep(timeout).await;
                        let _ = timeout_sender.send(sub_id).await;
                    });
                }

                let sub_id = subscription.id.clone();
                let msg = ClientMessage::new_req(sub_id.clone(), subscription.filters.clone());

                log::debug!("{}", msg.as_json());

                self.send_ws(ws_write, msg).await?;

                self.subscriptions.insert(sub_id.clone(), subscription);

                Ok(Some(RelayEvent::RelayNotification(
                    RelayNotificationEvent::SentSubscription(sub_id),
                )))
            }
            RelayInput::Count(subscription) => {
                log::debug!("Sending count request");
                let msg = ClientMessage::new_count(subscription.id.clone(), subscription.filters);
                log::debug!("{}", msg.as_json());
                self.send_ws(ws_write, msg).await?;
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
        &self,
        ws_write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
        msg: ClientMessage,
    ) -> Result<(), Error> {
        if !self.write {
            return Err(Error::BlockedWriteToRelay(self.url.to_owned()));
        }
        ws_write
            .send(Message::text(msg.as_json()))
            .await
            .map_err(|e| Error::FailedToSendToSocket(e.to_string()))?;
        Ok(())
    }
}

pub enum RelayState {
    Connected {
        ws_read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        ws_write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    },
    Connecting {
        one_rx: mpsc::Receiver<ConnResult>,
        delay_to_connect_millis: u64,
    },
    Disconnected {
        delay_to_connect_millis: u64,
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
        RelayState::Disconnected {
            delay_to_connect_millis: 0,
        }
    }
    pub fn retry_with_delay(delay_to_connect_millis: u64) -> Self {
        let delay_to_connect_millis = if delay_to_connect_millis == 0 {
            1000
        } else {
            delay_to_connect_millis * 2
        };
        RelayState::Disconnected {
            delay_to_connect_millis,
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

#[derive(Debug, Clone)]
pub enum RelayInput {
    ToggleRead(bool),
    ToggleWrite(bool),
    SendEvent(nostr::Event),
    Subscribe(Subscription),
    Close,
    Reconnect,
    Count(Subscription),
    CloseSubscription(SubscriptionId),
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
    Timeout(SubscriptionId),
    RelayMessage(RelayMessage),
    SentSubscription(SubscriptionId),
    SentEvent(nostr::EventId),
}
impl RelayNotificationEvent {
    pub fn to_event(self: RelayNotificationEvent, url: &Url) -> NotificationEvent {
        match self {
            RelayNotificationEvent::RelayMessage(message) => {
                NotificationEvent::RelayMessage(url.to_owned(), message)
            }
            RelayNotificationEvent::SentSubscription(sub_id) => {
                NotificationEvent::SentSubscription(url.to_owned(), sub_id)
            }
            RelayNotificationEvent::SentEvent(event_id) => {
                NotificationEvent::SentEvent(url.to_owned(), event_id)
            }
            RelayNotificationEvent::Timeout(subscription_id) => {
                NotificationEvent::Timeout(url.to_owned(), subscription_id)
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
                    delay_to_connect_millis,
                },
            )
        }
        RelayState::Connecting {
            mut one_rx,
            delay_to_connect_millis,
        } => {
            let event_opt = tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(GET_CONN_DELAY_MILLIS)) => {
                        log::trace!("{} - waiting", &relay.url);
                        None
                }
                received = one_rx.recv() => {
                    match received {
                        Some(Ok(ws_stream)) => {
                            log::debug!("{} - CONN OK", &relay.url);

                            let (ws_write, ws_read) = ws_stream.split();

                            log::info!("{} - subscriptions {}", &relay.url, relay.subscriptions.len());

                            relay.stats.new_success();

                            return (
                                Some(RelayEvent::RelayToPool(RelayToPoolEvent::ConnectedToSocket)),
                                RelayState::Connected {
                                    ws_read,
                                    ws_write,
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
                                            RelayState::retry_with_delay(delay_to_connect_millis)
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
                    delay_to_connect_millis,
                },
            )
        }
        RelayState::Connected {
            mut ws_read,
            mut ws_write,
        } => {
            // Create future depending on whether relay.read is true or false
            let future = if relay.read {
                Either::Left(ws_read.next())
            } else {
                log::debug!("{}", Error::BlockedReadFromRelay(relay.url.to_owned()));
                Either::Right(pending())
            };

            // Receive subscription response
            let event_opt = tokio::select! {
                message = future => {
                    handle_ws_message(&relay.url, message)
                }
                input = relay.relay_input_receiver.recv() => {
                    if let Some(input) = input {
                        match relay.handle_input(&mut ws_write, input).await {
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
                timeout = relay.timeout_rx.recv() => {
                    if let Some(sub_id) = timeout {
                        if relay.subscriptions.get(&sub_id).is_some() {
                            log::debug!("{}: Timeout for subscription {}", &relay.url, sub_id);
                            Some(RelayEvent::RelayNotification(RelayNotificationEvent::Timeout(sub_id)))
                        } else {
                            None
                        }
                    } else {
                        log::debug!("{}: Relay input channel closed", &relay.url);
                        None
                    }
                }
            };

            if let Some(RelayEvent::RelayToPool(RelayToPoolEvent::Closed)) = event_opt {
                return (
                    Some(RelayEvent::RelayToPool(RelayToPoolEvent::Closed)),
                    RelayState::Terminated,
                );
            }

            (event_opt, RelayState::Connected { ws_read, ws_write })
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
const MAX_ATTEMPS: usize = 5;

#[derive(Debug, Clone)]
pub struct Subscription {
    pub id: SubscriptionId,
    pub filters: Vec<Filter>,
    pub sub_type: SubscriptionType,
    pub status: SubscriptionStatus,
    pub timeout: Option<Duration>,
}
impl Subscription {
    pub fn new(filters: Vec<Filter>, sub_type: SubscriptionType) -> Self {
        Self {
            id: SubscriptionId::generate(),
            filters,
            sub_type,
            status: SubscriptionStatus::Active,
            timeout: None,
        }
    }
    pub fn with_id(mut self, id: &SubscriptionId) -> Self {
        self.id = id.to_owned();
        self
    }
    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }
    pub fn done(&mut self) {
        self.status = SubscriptionStatus::Done;
    }
}
#[derive(Debug, Clone)]
pub enum SubscriptionType {
    Count,
    UntilEOSE,
    Endless,
}
#[derive(Debug, Clone)]
pub enum SubscriptionStatus {
    Active,
    Done,
}
