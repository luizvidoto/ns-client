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
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::error::Error as WsError;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use url::Url;

use crate::utils::spawn_get_connection;
use crate::utils::ConnResult;
use crate::Error;
use crate::RelayConnectionStats;

pub struct RelayPoolTask {
    outside_rx: mpsc::UnboundedReceiver<PoolInput>,
    notification_sender: broadcast::Sender<NotificationEvent>,
    to_pool_tx: mpsc::Sender<RelayToPool>,
    to_pool_rx: mpsc::Receiver<RelayToPool>,
    relays: HashMap<Url, mpsc::Sender<RelayInput>>,
    subscriptions: HashMap<SubscriptionId, Vec<Filter>>,
}
impl RelayPoolTask {
    pub fn new(
        outside_rx: mpsc::UnboundedReceiver<PoolInput>,
        notification_sender: broadcast::Sender<NotificationEvent>,
    ) -> Self {
        let (to_pool_tx, to_pool_rx) = mpsc::channel(RELAY_TO_POOL_CHANNEL_SIZE);
        let relays = HashMap::new();
        let subscriptions = HashMap::new();
        Self {
            outside_rx,
            notification_sender,
            to_pool_rx,
            to_pool_tx,
            relays,
            subscriptions,
        }
    }
    pub async fn run(&mut self) {
        loop {
            let event_opt = tokio::select! {
                relay_input = self.to_pool_rx.recv() => {
                    if let Some(msg) = relay_input {
                        match msg.event {
                            RelayToPoolEvent::ConnectedToSocket => {
                                log::info!("{} - Connected to socket", &msg.url);
                                for (sub_id, filters) in &self.subscriptions {
                                    if let Some(relay_tx) = self.relays.get_mut(&msg.url) {
                                        let relay_tx_1 = relay_tx.clone();
                                        let sub_id_1 = sub_id.clone();
                                        let filters_1 = filters.clone();
                                        tokio::spawn(async move {
                                            if let Err(e) = relay_tx_1.send(RelayInput::Subscribe((sub_id_1, filters_1))).await {
                                                log::debug!("relay dropped: {}", e);
                                            }
                                        });
                                    }
                                }
                                None
                            },
                            RelayToPoolEvent::Reconnecting => None,
                            RelayToPoolEvent::FailedToConnect => None,
                            RelayToPoolEvent::RelayDisconnected => None,
                            RelayToPoolEvent::AttempToConnect => None,
                            RelayToPoolEvent::MaxAttempsExceeded => Some(NotificationEvent::RelayTerminated(msg.url.clone())),
                            RelayToPoolEvent::TerminateRelay => Some(NotificationEvent::RelayTerminated(msg.url.clone())),
                            RelayToPoolEvent::Closed => None,
                        }
                    } else {
                        log::debug!("Relay pool task channel closed");
                        None
                    }
                }
                pool_input = self.outside_rx.recv() => {
                    if let Some(input) = pool_input {
                        match input {
                            PoolInput::Shutdown => {
                                for (_url, relay_tx) in &mut self.relays{
                                    if let Err(e) = relay_tx.send(RelayInput::Close).await {
                                        log::debug!("Failed to send close to relay: {}", e);
                                    }
                                }
                                break;
                            }
                            PoolInput::AddRelay((url, _opts)) => {
                                // TODO: use opts
                                process_add_relay(&self.to_pool_tx, &self.notification_sender, url, &mut self.relays).await
                            }
                            PoolInput::SendEvent(event) => {
                                for (_url, relay_tx) in &mut self.relays{
                                    if let Err(e) = relay_tx.send(RelayInput::SendEvent(event.clone())).await {
                                        log::debug!("Failed to send event to relay: {}", e);
                                    }
                                }
                                None
                            }
                            PoolInput::ReconnectRelay(url) => {
                                process_reconnect_relay(url, &mut self.relays).await
                            }
                            PoolInput::AddSubscription((sub_id, filters)) => {
                                // process_subscribe_all(sub_id, filters, &mut self.relays).await
                                self.subscriptions.insert(sub_id.clone(), filters);
                                None
                            }
                            PoolInput::RemoveRelay(url) => {
                                if let Some(relay_tx) = &mut self.relays.get_mut(&url) {
                                    if let Err(e) = relay_tx.send(RelayInput::Close).await{
                                        log::debug!("Failed to send event to relay: {}", e);
                                    }
                                }
                                self.relays.remove(&url);
                                None
                            }
                            PoolInput::ToggleReadFor((url, read)) => {
                                todo!()
                            }
                            PoolInput::ToggleWriteFor((url, write)) => {
                                todo!()
                            }
                        }
                    } else {
                        log::debug!("Outside channel closed");
                        None
                    }
                }
            };

            if let Some(event) = event_opt {
                if let Err(e) = self.notification_sender.send(event) {
                    log::debug!("Failed to send pool event: {}", e);
                }
            }
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
pub struct RelayPool {
    notification_sender: broadcast::Sender<NotificationEvent>,
    outside_tx: mpsc::UnboundedSender<PoolInput>,
}
impl RelayPool {
    pub fn new() -> Self {
        let (outside_tx, outside_rx) = mpsc::unbounded_channel();
        let (notification_sender, _) = broadcast::channel(EVENT_SENDER_CHANNEL_SIZE);

        let mut relay_pool_task = RelayPoolTask::new(outside_rx, notification_sender.clone());
        tokio::spawn(async move {
            relay_pool_task.run().await;
        });
        Self {
            outside_tx,
            notification_sender,
        }
    }
    pub fn notifications(&self) -> broadcast::Receiver<NotificationEvent> {
        self.notification_sender.subscribe()
    }
    pub fn subscribe(&self, filters: Vec<Filter>) -> Result<SubscriptionId, Error> {
        let sub_id = SubscriptionId::generate();
        let msg = PoolInput::AddSubscription((sub_id.clone(), filters));
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(sub_id)
    }
    pub fn reconnect_relay(&self, url: &Url) -> Result<(), Error> {
        let msg = PoolInput::ReconnectRelay(url.to_owned());
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))
    }
    pub fn add_relay(&self, url: &str) -> Result<Url, Error> {
        log::trace!("Adding relay {}", url);
        let url = Url::parse(url)?;
        let opts = RelayOptions::default();
        let msg = PoolInput::AddRelay((url.clone(), opts));
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(url)
    }
    pub fn add_relay_with_opts(&self, url: &str, opts: RelayOptions) -> Result<Url, Error> {
        log::trace!("Adding relay {}", url);
        let url = Url::parse(url)?;
        let msg = PoolInput::AddRelay((url.clone(), opts));
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(url)
    }
    pub fn shutdown(&self) -> Result<(), Error> {
        log::info!("shutdown client");
        let msg = PoolInput::Shutdown;
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(())
    }
    pub fn send_event(&self, event: nostr::Event) -> Result<(), Error> {
        log::info!("send event to relays");
        let msg = PoolInput::SendEvent(event);
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(())
    }
    pub fn remove_relay(&self, url: &str) -> Result<(), Error> {
        log::info!("remove relay {}", url);
        let url = Url::parse(url)?;
        let msg = PoolInput::RemoveRelay(url);
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(())
    }
    pub fn toggle_read_for(&self, url: &Url, read: bool) -> Result<(), Error> {
        log::info!("toggle write for {} - read: {}", url, read);
        let msg = PoolInput::ToggleReadFor((url.to_owned(), read));
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(())
    }
    pub fn toggle_write_for(&self, url: &Url, write: bool) -> Result<(), Error> {
        log::info!("toggle write for {} - write: {}", url, write);
        let msg = PoolInput::ToggleWriteFor((url.to_owned(), write));
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum PoolInput {
    AddRelay((Url, RelayOptions)),
    ReconnectRelay(Url),
    AddSubscription((SubscriptionId, Vec<Filter>)),
    Shutdown,
    SendEvent(nostr::Event),
    RemoveRelay(Url),
    ToggleReadFor((Url, bool)),
    ToggleWriteFor((Url, bool)),
    // AddSubscriptionTo {
    //     subscription_id: SubscriptionId,
    //     url: Url,
    //     filters: Vec<Filter>,
    // },
}
#[derive(Debug, Clone)]
pub enum NotificationEvent {
    // RelayAdded(Url),
    // RelayConnected(Url),
    // RelayConnecting(Url),
    // RelayDisconnected(Url),
    RelayTerminated(Url),
    RelayMessage((Url, RelayMessage)),
    SentSubscription((Url, SubscriptionId)),
    SentEvent((Url, nostr::EventId)),
}

pub enum RelayState {
    Connected {
        url: Url,
        ws_read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        ws_write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
        stats: RelayConnectionStats,
        relay_input_receiver: mpsc::Receiver<RelayInput>,
        input_queue: VecDeque<RelayInput>,
    },
    Connecting {
        url: Url,
        stats: RelayConnectionStats,
        one_rx: mpsc::Receiver<ConnResult>,
        relay_input_receiver: mpsc::Receiver<RelayInput>,
        input_queue: VecDeque<RelayInput>,
        delay_to_connect_millis: u64,
    },
    Disconnected {
        url: Url,
        stats: RelayConnectionStats,
        relay_input_receiver: mpsc::Receiver<RelayInput>,
        delay_to_connect_millis: u64,
        input_queue: VecDeque<RelayInput>,
    },
    Terminated {
        url: Url,
        stats: RelayConnectionStats,
        relay_input_receiver: mpsc::Receiver<RelayInput>,
    },
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
#[derive(Debug, Clone, Default)]
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
    pub fn new(url: &Url, relay_input_receiver: mpsc::Receiver<RelayInput>) -> Self {
        let input_queue = VecDeque::new();
        RelayState::Disconnected {
            url: url.to_owned(),
            stats: RelayConnectionStats::new(),
            relay_input_receiver,
            delay_to_connect_millis: 0,
            input_queue,
        }
    }
    pub fn retry_with_delay(
        url: Url,
        stats: RelayConnectionStats,
        relay_input_receiver: mpsc::Receiver<RelayInput>,
        delay_to_connect_millis: u64,
        input_queue: VecDeque<RelayInput>,
    ) -> Self {
        let delay_to_connect_millis = if delay_to_connect_millis == 0 {
            1000
        } else {
            delay_to_connect_millis * 2
        };
        RelayState::Disconnected {
            url,
            stats,
            relay_input_receiver,
            delay_to_connect_millis,
            input_queue,
        }
    }
}

#[derive(Debug, Clone)]
pub enum RelayInput {
    SendEvent(nostr::Event),
    Subscribe((SubscriptionId, Vec<Filter>)),
    Close,
    Reconnect,
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
                Some(NotificationEvent::RelayMessage((url.to_owned(), message)))
            }
            RelayNotificationEvent::SentSubscription(sub_id) => Some(
                NotificationEvent::SentSubscription((url.to_owned(), sub_id)),
            ),
            RelayNotificationEvent::SentEvent(event_id) => {
                Some(NotificationEvent::SentEvent((url.to_owned(), event_id)))
            }
        }
    }
}
#[derive(Debug, Clone)]
pub struct RelayToPool {
    url: Url,
    event: RelayToPoolEvent,
}

pub async fn run_relay(state: RelayState) -> (Option<RelayEvent>, RelayState) {
    match state {
        RelayState::Terminated {
            url,
            stats,
            mut relay_input_receiver,
        } => {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(RECONNECT_DELAY_SECS)) => {
                        (None, RelayState::Terminated { url, stats, relay_input_receiver })
                }
                input = relay_input_receiver.recv() => {
                    if let Some(input) = input {
                        if let RelayInput::Reconnect = input {
                            return (Some(
                                RelayEvent::RelayToPool(
                                    RelayToPoolEvent::Reconnecting
                                ))
                            , RelayState::new(&url, relay_input_receiver))
                        } else {
                            log::debug!("{} - other input - {:?}", &url, input);
                        }
                    } else {
                        log::debug!("{} - relay input receiver closed", &url);
                    }
                    (None, RelayState::Terminated { url, stats, relay_input_receiver })
                }
            }
        }
        RelayState::Disconnected {
            url,
            stats,
            relay_input_receiver,
            delay_to_connect_millis,
            input_queue,
        } => {
            stats.new_attempt();
            let attempts = stats.attempts();

            if attempts >= MAX_ATTEMPS {
                log::trace!("{} - max attempts exceeded - {}", &url, attempts);
                return (
                    Some(RelayEvent::RelayToPool(
                        RelayToPoolEvent::MaxAttempsExceeded,
                    )),
                    RelayState::Terminated {
                        url,
                        stats,
                        relay_input_receiver,
                    },
                );
            }

            tokio::time::sleep(Duration::from_millis(delay_to_connect_millis)).await;

            let (one_tx, one_rx) = mpsc::channel(1);
            spawn_get_connection(url.clone(), one_tx);

            (
                Some(RelayEvent::RelayToPool(RelayToPoolEvent::AttempToConnect)),
                RelayState::Connecting {
                    url,
                    stats,
                    one_rx,
                    input_queue,
                    relay_input_receiver,
                    delay_to_connect_millis,
                },
            )
        }
        RelayState::Connecting {
            url,
            stats,
            mut one_rx,
            mut relay_input_receiver,
            mut input_queue,
            delay_to_connect_millis,
        } => {
            let event_opt = tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(GET_CONN_DELAY_MILLIS)) => {
                        log::trace!("{} - waiting", &url);
                        None
                }
                input = relay_input_receiver.recv() => {
                    if let Some(input) = input {
                        input_queue.push_front(input);
                        log::debug!("{} - input q: {}", &url, input_queue.len());
                    }
                    None
                }
                received = one_rx.recv() => {
                    match received {
                        Some(Ok(ws_stream)) => {
                            log::debug!("{} - CONN OK", &url);
                            let (ws_write, ws_read) = ws_stream.split();
                            return (
                                Some(RelayEvent::RelayToPool(RelayToPoolEvent::ConnectedToSocket)),
                                RelayState::Connected {
                                    url,
                                    ws_read,
                                    ws_write,
                                    stats,
                                    relay_input_receiver,
                                    input_queue
                                },
                            );
                        }
                        Some(Err(e)) => {
                            if let Some(tungstenite_error) = e.downcast_ref::<tungstenite::Error>() {
                                match tungstenite_error {
                                    tungstenite::Error::Io(_) => {
                                        log::trace!("{} - retry with delay", &url);
                                        return (
                                            Some(RelayEvent::RelayToPool(RelayToPoolEvent::FailedToConnect)),
                                            RelayState::retry_with_delay(url, stats, relay_input_receiver, delay_to_connect_millis, input_queue)
                                        )
                                    },
                                    other => log::info!("{} - tungstenite error: {}", &url, other),
                                }
                            }
                            log::info!("{} - conn err: {}", &url, e);
                        }
                        None => {
                            log::debug!("{} - one_rx closed", &url);
                        }
                    }
                    return (
                        Some(RelayEvent::RelayToPool(RelayToPoolEvent::TerminateRelay)),
                        RelayState::Terminated { url, stats, relay_input_receiver },
                    );
                }
            };
            (
                event_opt,
                RelayState::Connecting {
                    url,
                    stats,
                    one_rx,
                    relay_input_receiver,
                    input_queue,
                    delay_to_connect_millis,
                },
            )
        }
        RelayState::Connected {
            url,
            mut ws_read,
            mut ws_write,
            mut relay_input_receiver,
            stats,
            mut input_queue,
        } => {
            stats.new_success();
            let event_opt = if let Some(input) = input_queue.pop_front() {
                match handle_relay_input(&mut ws_write, input).await {
                    Ok(event_opt) => event_opt,
                    Err(e) => {
                        log::debug!("{}: {}", &url, e);
                        None
                    }
                }
            } else {
                // Receive subscription response
                let event = tokio::select! {
                    message = ws_read.next() => {
                        handle_ws_message(&url, message)
                    }
                    input = relay_input_receiver.recv() => {
                        if let Some(input) = input {
                            match handle_relay_input(&mut ws_write, input).await {
                                Ok(event_opt) => event_opt,
                                Err(e) => {
                                    log::debug!("{}: {}", &url, e);
                                    None
                                }
                            }
                        } else {
                            log::debug!("{}: Relay input channel closed", &url);
                            None
                        }
                    }
                };
                event
            };

            if let Some(RelayEvent::RelayToPool(RelayToPoolEvent::Closed)) = event_opt {
                return (
                    Some(RelayEvent::RelayToPool(RelayToPoolEvent::Closed)),
                    RelayState::Terminated {
                        url,
                        stats,
                        relay_input_receiver,
                    },
                );
            }

            (
                event_opt,
                RelayState::Connected {
                    stats,
                    url,
                    ws_read,
                    ws_write,
                    relay_input_receiver,
                    input_queue,
                },
            )
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
            ws_write
                .send(Message::text(msg.as_json()))
                .await
                .map_err(|e| Error::FailedToSendToSocket(e.to_string()))?;
            log::debug!("Sent subscription request");
            Ok(Some(RelayEvent::RelayNotification(
                RelayNotificationEvent::SentEvent(event_id),
            )))
        }
        RelayInput::Subscribe((sub_id, filters)) => {
            log::debug!("Sending subscription request");
            let msg = ClientMessage::new_req(sub_id.clone(), filters);
            ws_write
                .send(Message::text(msg.as_json()))
                .await
                .map_err(|e| Error::FailedToSendToSocket(e.to_string()))?;
            log::debug!("Sent subscription request");
            Ok(Some(RelayEvent::RelayNotification(
                RelayNotificationEvent::SentSubscription(sub_id),
            )))
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

async fn process_add_relay(
    to_pool_sender: &mpsc::Sender<RelayToPool>,
    pool_event_sender: &broadcast::Sender<NotificationEvent>,
    url: Url,
    relays: &mut HashMap<Url, mpsc::Sender<RelayInput>>,
) -> Option<NotificationEvent> {
    let (relay_input_sender, relay_input_receiver) = mpsc::channel(RELAY_INPUT_CHANNEL_SIZE);
    let relay_task = spawn_relay_task(
        url.clone(),
        to_pool_sender.clone(),
        relay_input_sender.clone(),
        relay_input_receiver,
        pool_event_sender.clone(),
    );
    tokio::spawn(relay_task);
    relays.insert(url.clone(), relay_input_sender);
    // Some(NotificationEvent::RelayAdded(url))
    None
}

async fn process_reconnect_relay(
    url: Url,
    relays: &mut HashMap<Url, mpsc::Sender<RelayInput>>,
) -> Option<NotificationEvent> {
    if let Some(relay_tx) = relays.get_mut(&url) {
        if let Err(e) = relay_tx.try_send(RelayInput::Reconnect) {
            log::debug!("Failed to send reconnect to relay: {}", e);
        }
    } else {
        log::warn!("Relay not found");
    }
    None
}

// async fn process_subscribe_to(
//     url: Url,
//     sub_id: SubscriptionId,
//     filters: Vec<Filter>,
//     relays: &mut HashMap<Url, mpsc::Sender<RelayInput>>,
// ) -> Option<NotificationEvent> {
//     if let Some(relay_tx) = relays.get_mut(&url) {
//         if let Err(e) = relay_tx.try_send(RelayInput::Subscribe((sub_id, filters))) {
//             log::error!("Failed to send subscription to relay: {}", e);
//         }
//     } else {
//         log::warn!("Relay not found");
//     }
//     None
// }

// async fn process_subscribe_all(
//     sub_id: SubscriptionId,
//     filters: Vec<Filter>,
//     relays: &mut HashMap<Url, mpsc::Sender<RelayInput>>,
// ) -> Option<NotificationEvent> {
//     if let Some(relay_tx) = relays.get_mut(&url) {
//         if let Err(e) = relay_tx.try_send(RelayInput::Subscribe((sub_id, filters))) {
//             log::error!("Failed to send subscription to relay: {}", e);
//         }
//     } else {
//         log::warn!("Relay not found");
//     }
//     None
// }

async fn spawn_relay_task(
    url: Url,
    pool_tx: mpsc::Sender<RelayToPool>,
    relay_input_sender: mpsc::Sender<RelayInput>,
    relay_input_receiver: mpsc::Receiver<RelayInput>,
    notification_sender: broadcast::Sender<NotificationEvent>,
) {
    let mut state = RelayState::new(&url, relay_input_receiver);
    loop {
        let (event_opt, new_state) = run_relay(state).await;
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

const EVENT_SENDER_CHANNEL_SIZE: usize = 10000;
const RELAY_TO_POOL_CHANNEL_SIZE: usize = 10;
const RELAY_INPUT_CHANNEL_SIZE: usize = 10;

const GET_CONN_DELAY_MILLIS: u64 = 100;
const RECONNECT_DELAY_SECS: u64 = 2;
const MAX_ATTEMPS: usize = 3;
