use futures_util::future::pending;
use futures_util::future::Either;
use futures_util::future::Pending;
use futures_util::stream::SplitSink;
use futures_util::stream::SplitStream;
use futures_util::SinkExt;
use futures_util::StreamExt;
use nostr::prelude::RelayInformationDocument;
use nostr::ClientMessage;
use nostr::EventId;
use nostr::Filter;
use nostr::RelayMessage;
use nostr::SubscriptionId;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::Duration;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::oneshot::error::RecvError;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use url::Url;

use crate::pool::RelayToPool;
use crate::utils::spawn_get_connection;
use crate::utils::spawn_get_document;
use crate::utils::ConnResult;
use crate::utils::DocResult;
use crate::NotificationEvent;
use crate::RelayConnectionStats;

#[derive(Error, Debug, Clone)]
pub enum Error {
    #[error("Failed to send to websocket: {0}")]
    FailedToSendToSocket(String),

    #[error("Can't write to relay: {0}")]
    BlockedWriteToRelay(Url),

    #[error("Can't read from relay: {0}")]
    BlockedReadFromRelay(Url),

    #[error("{0}")]
    FromRelayError(#[from] SendError),
}

#[derive(Error, Debug, Clone)]
pub enum SendError {
    #[error("Failed to close subscription. ID: {0}")]
    FailedToCloseSubscription(SubscriptionId),

    #[error("Failed to send count subscription. ID: {0}")]
    FailedToSendCount(SubscriptionId),

    #[error("Failed to send event. ID: {0}")]
    FailedToSendEvent(nostr::EventId),

    #[error("Failed to subscribe. ID: {0}")]
    FailedToSendSubscription(SubscriptionId),
}

pub enum DocRequestState {
    Initial,
    Requested(oneshot::Receiver<DocResult>),
    Received,
    Failed,
}
impl DocRequestState {
    pub fn get_fut(
        &mut self,
    ) -> Either<&mut oneshot::Receiver<DocResult>, Pending<Result<DocResult, RecvError>>> {
        match self {
            DocRequestState::Requested(doc_rx) => Either::Left(doc_rx),
            _ => Either::Right(pending()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RelayErrorMessage {
    pub event_hash: EventId,
    pub message: String,
    pub date_milliseconds: u64,
}
impl RelayErrorMessage {
    pub fn new(event_hash: EventId, message: String) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before Unix epoch");
        Self {
            event_hash,
            message,
            date_milliseconds: now.as_millis() as u64,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RelayInformation {
    pub error_messages: VecDeque<RelayErrorMessage>, // Use VecDeque instead of Vec
    pub document: Option<RelayInformationDocument>,
    pub status: RelayStatus,
    pub conn_stats: RelayConnectionStats,
}

impl RelayInformation {
    pub fn new() -> Self {
        Self {
            error_messages: VecDeque::new(), // Initialize with VecDeque
            document: None,
            status: RelayStatus::Disconnected,
            conn_stats: RelayConnectionStats::new(),
        }
    }

    pub fn push_error(&mut self, error_msg: RelayErrorMessage) {
        self.error_messages.push_back(error_msg);

        // If there are more than 100 error messages, remove the oldest one
        if self.error_messages.len() as u8 > MAX_ERROR_MSGS_LIMIT {
            self.error_messages.pop_front(); // Removes the oldest message
        }
    }
}

pub struct Relay {
    url: Url,
    read: bool,
    write: bool,
    information: RelayInformation,
    document_request: DocRequestState,
    input: mpsc::Receiver<RelayInput>,
    pool_tx: mpsc::Sender<RelayToPool>,
    notification_tx: broadcast::Sender<NotificationEvent>,
    timeout_tx: mpsc::Sender<SubscriptionId>,
    timeout_rx: mpsc::Receiver<SubscriptionId>,
    subscriptions: HashMap<SubscriptionId, Subscription>,
    eose_actions: HashMap<String, BTreeMap<SubscriptionId, Subscription>>,
    input_queue: VecDeque<RelayInput>,
}
impl Relay {
    pub fn new(
        url: &Url,
        input: mpsc::Receiver<RelayInput>,
        pool_tx: mpsc::Sender<RelayToPool>,
        notification_tx: broadcast::Sender<NotificationEvent>,
        opts: RelayOptions,
    ) -> Self {
        let (timeout_tx, timeout_rx) = mpsc::channel(10);

        Self {
            input,
            information: RelayInformation::new(),
            url: url.to_owned(),
            subscriptions: HashMap::new(),
            read: opts.read,
            write: opts.write,
            timeout_rx,
            timeout_tx,
            pool_tx,
            notification_tx,
            document_request: DocRequestState::Initial,
            eose_actions: HashMap::new(),
            input_queue: VecDeque::new(),
        }
    }
    fn set_document(&mut self, document: &RelayInformationDocument) {
        self.document_request = DocRequestState::Received;
        self.information.document = Some(document.to_owned());
    }
    fn attempts(&self) -> usize {
        self.information.conn_stats.attempts()
    }
    fn new_attempt(&mut self) {
        self.information.status = RelayStatus::Connecting;
        self.information.conn_stats.new_attempt();
    }
    fn new_success(&mut self) {
        self.information.status = RelayStatus::Connected;
        self.information.conn_stats.new_success()
    }
    fn disconnected(&mut self) {
        self.information.status = RelayStatus::Disconnected;
    }
    fn terminated(&mut self) {
        self.information.status = RelayStatus::Terminated;
    }
    fn handle_input_disconnected(&mut self, input: RelayInput) {
        match input {
            RelayInput::FetchInformation => {
                let information = self.information.clone();
                _ = self
                    .notification_tx
                    .send(RelayEvent::RelayInformation(information).into(&self.url));
            }
            RelayInput::ToggleRead(read) => {
                self.read = read;
            }
            RelayInput::ToggleWrite(write) => {
                self.write = write;
            }
            RelayInput::Close => {
                unreachable!("Close should be handled outside this function")
            }
            RelayInput::Reconnect => {
                unreachable!("Reconnect should be handled outside this function")
            }
            other => {
                // send to a queue to be handled when connected
                self.input_queue.push_back(other);
            }
        }
    }

    async fn send_event(
        &self,
        event: nostr::Event,
        ws_write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    ) -> Result<(), SendError> {
        log::debug!("Sending event");
        let event_id = event.id;
        let msg = ClientMessage::new_event(event);
        log::debug!("{}", msg.as_json());
        self.send_ws(ws_write, msg)
            .await
            .map_err(|_| SendError::FailedToSendEvent(event_id))?;
        _ = self
            .notification_tx
            .send(RelayEvent::SentEvent(event_id).into(&self.url));
        Ok(())
    }

    async fn subscribe(
        &mut self,
        subscription: Subscription,
        ws_write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    ) -> Result<(), SendError> {
        log::debug!("Sending subscription request");
        if self.subscriptions.contains_key(&subscription.id) {
            log::info!("Already subscribed");
            return Ok(());
        }

        let sub_id = subscription.id.clone();
        if let Some(timeout) = subscription.timeout {
            let timeout_sender = self.timeout_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(timeout).await;
                _ = timeout_sender.send(sub_id).await;
            });
        }

        let sub_id = subscription.id.clone();
        let msg = ClientMessage::new_req(sub_id.clone(), subscription.filters.clone());

        log::debug!("{}", msg.as_json());

        self.send_ws(ws_write, msg)
            .await
            .map_err(|_| SendError::FailedToSendSubscription(sub_id.clone()))?;

        match subscription.sub_type {
            SubscriptionType::Action => (),
            _ => {
                self.subscriptions.insert(sub_id.clone(), subscription);
            }
        }

        _ = self
            .notification_tx
            .send(RelayEvent::SentSubscription(sub_id).into(&self.url));
        Ok(())
    }

    async fn count(
        &self,
        subscription: Subscription,
        ws_write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    ) -> Result<(), SendError> {
        log::debug!("Sending count request");
        let sub_id = subscription.id.clone();
        let msg = ClientMessage::new_count(sub_id.clone(), subscription.filters);
        log::debug!("{}", msg.as_json());
        self.send_ws(ws_write, msg)
            .await
            .map_err(|_| SendError::FailedToSendCount(sub_id.clone()))?;
        _ = self
            .notification_tx
            .send(RelayEvent::SentCount(sub_id).into(&self.url));
        Ok(())
    }

    async fn close_subscription(
        &mut self,
        sub_id: &SubscriptionId,
        ws_write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    ) -> Result<(), SendError> {
        log::debug!("Sending close subscription");
        let msg = ClientMessage::close(sub_id.clone());
        log::debug!("{}", msg.as_json());
        self.send_ws(ws_write, msg)
            .await
            .map_err(|_| SendError::FailedToCloseSubscription(sub_id.to_owned()))?;
        self.subscriptions.remove(sub_id);
        Ok(())
    }

    fn find_subscription(
        &mut self,
        sub_id: &SubscriptionId,
    ) -> (Option<String>, Option<Subscription>) {
        for (action_id, actions) in &mut self.eose_actions {
            if actions.contains_key(sub_id) {
                let removed_sub = actions.remove(sub_id);
                return (Some(action_id.clone()), removed_sub);
            }
        }
        (None, None)
    }

    async fn check_ok_msg(&mut self, message: &RelayMessage) {
        if let RelayMessage::Ok {
            event_id: event_hash,
            status,
            message: error_msg,
        } = message
        {
            // Status false means that some event was not accepted by the relay for some reason
            if *status == false {
                let error_msg = RelayErrorMessage::new(*event_hash, error_msg.to_owned());
                self.information.push_error(error_msg.clone());
                // _ = self
                //     .pool_tx
                //     .send(RelayToPoolEvent::RelayError(error_msg).into(&self.url))
                //     .await;
            }
        }
    }

    async fn check_eose(
        &mut self,
        message: &RelayMessage,
        ws_write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    ) -> Result<(), SendError> {
        if let RelayMessage::EndOfStoredEvents(sub_id) = &message {
            // Find action_id and subscription or dont find anything
            match self.find_subscription(&sub_id) {
                (Some(action_id), Some(removed_sub)) => {
                    self.close_subscription(&removed_sub.id, ws_write).await?;
                    let next_subscription = self
                        .eose_actions
                        .get_mut(&action_id)
                        .and_then(|actions| actions.first_key_value())
                        .map(|(_id, subscription)| subscription.to_owned());

                    if let Some(subscription) = next_subscription {
                        self.subscribe(subscription, ws_write).await?;
                    } else {
                        _ = self
                            .notification_tx
                            .send(RelayEvent::ActionsDone(action_id.clone()).into(&self.url));
                    }
                    return Ok(());
                }
                _ => (),
            }

            if let Some(subscription) = self.subscriptions.get(&sub_id) {
                if let SubscriptionType::UntilEOSE = subscription.sub_type {
                    self.close_subscription(&sub_id, ws_write).await?;
                }
            }
        }
        Ok(())
    }

    async fn handle_terminated(&mut self, state: RelayState) -> (LoopControl, RelayState) {
        if let Some(input) = self.input.recv().await {
            match input {
                RelayInput::Close => {
                    // ignore close when terminated
                    log::debug!("Ignoring Closing relay. Relay is terminated");
                }
                RelayInput::Reconnect => {
                    self.disconnected();
                    _ = self
                        .pool_tx
                        .send(RelayToPoolEvent::Reconnecting.into(&self.url))
                        .await;
                    return (LoopControl::Continue, RelayState::new());
                }
                other => self.handle_input_disconnected(other),
            }
        } else {
            log::debug!("{} - relay input receiver closed", &self.url);
            return (LoopControl::Break, state);
        }
        (LoopControl::Continue, state)
    }

    async fn handle_disconnected(&mut self, mut state: RelayState) -> (LoopControl, RelayState) {
        if let RelayState::Disconnected {
            delay_to_connect_millis,
        } = &mut state
        {
            self.new_attempt();

            let attempts = self.attempts();
            if attempts >= MAX_ATTEMPS {
                log::debug!("{} - max attempts exceeded - {}", &self.url, attempts);
                _ = self
                    .pool_tx
                    .send(RelayToPoolEvent::MaxAttempsExceeded.into(&self.url))
                    .await;
                return (LoopControl::Continue, RelayState::Terminated);
            }

            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(*delay_to_connect_millis)) => (),
                input = self.input.recv() => {
                    if let Some(input) = input {
                        match input {
                            RelayInput::Close => {
                                log::debug!("Closing relay");
                                _ = self
                                    .pool_tx
                                    .send(RelayToPoolEvent::Closed.into(&self.url))
                                    .await;
                                self.terminated();
                                return (LoopControl::Continue, RelayState::Terminated);
                            }
                            RelayInput::Reconnect => {
                                // ignore reconnect when not terminated
                                log::debug!("Ignoring Reconnect relay. Relay is trying to connect");
                            }
                            other => self.handle_input_disconnected(other),
                        }
                    } else {
                        log::debug!("{} - relay input receiver closed", &self.url);
                        return (LoopControl::Break, state);
                    }
                }
            }

            let (conn_tx, conn_rx) = oneshot::channel();
            spawn_get_connection(self.url.clone(), conn_tx);

            _ = self
                .pool_tx
                .send(RelayToPoolEvent::AttempToConnect.into(&self.url))
                .await;

            (
                LoopControl::Continue,
                RelayState::Connecting {
                    conn_rx,
                    delay_to_connect_millis: *delay_to_connect_millis,
                },
            )
        } else {
            (LoopControl::Continue, state)
        }
    }

    async fn handle_connecting(&mut self, mut state: RelayState) -> (LoopControl, RelayState) {
        if let RelayState::Connecting {
            conn_rx,
            delay_to_connect_millis,
        } = &mut state
        {
            tokio::select! {
                input = self.input.recv() => {
                    if let Some(input) = input {
                        match input {
                            RelayInput::Close => {
                                log::debug!("Closing relay");
                                _ = self
                                    .pool_tx
                                    .send(RelayToPoolEvent::Closed.into(&self.url))
                                    .await;
                                self.terminated();
                                return (LoopControl::Continue, RelayState::Terminated);
                            }
                            RelayInput::Reconnect => {
                                // ignore reconnect when not terminated
                                log::debug!("Ignoring Reconnect relay. Relay is trying to connect");
                            }
                            other => self.handle_input_disconnected(other),
                        }
                    } else {
                        log::debug!("{} - relay input receiver closed", &self.url);
                        return (LoopControl::Break, state);
                    }
                }
                conn_result = conn_rx => {
                    match conn_result {
                        Ok(conn) => match conn {
                            Ok(ws_stream) => {
                                log::debug!(
                                    "{} - Ok. Subscriptions {}",
                                    &self.url,
                                    self.subscriptions.len()
                                );
                                let (ws_write, ws_read) = ws_stream.split();
                                self.new_success();
                                _ = self
                                    .pool_tx
                                    .send(RelayToPoolEvent::ConnectedToSocket.into(&self.url))
                                    .await;
                                return (LoopControl::Continue, RelayState::Connected { ws_read, ws_write })
                            }
                            Err(e) => {
                                log::debug!("{} - Retrying. Err: {}", &self.url, e);
                                _ = self
                                    .pool_tx
                                    .send(RelayToPoolEvent::FailedToConnect.into(&self.url))
                                    .await;
                                return (LoopControl::Continue, RelayState::retry_with_delay(*delay_to_connect_millis));
                            }
                        },
                        Err(e) => {
                            log::debug!("{} - Terminate. Err: {}", &self.url, e);
                            _ = self
                                .pool_tx
                                .send(RelayToPoolEvent::TerminateRelay.into(&self.url))
                                .await;
                            self.terminated();
                            return (LoopControl::Continue, RelayState::Terminated);
                        }
                    }
                }
            }
        }

        (LoopControl::Continue, state)
    }

    async fn handle_input_connected(
        &mut self,
        ws_write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
        input: RelayInput,
    ) -> Option<(LoopControl, RelayState)> {
        match input {
            RelayInput::Close => {
                log::debug!("Closing relay");
                _ = ws_write.close().await;
                _ = self
                    .pool_tx
                    .send(RelayToPoolEvent::Closed.into(&self.url))
                    .await;
                self.terminated();
                return Some((LoopControl::Continue, RelayState::Terminated));
            }
            other => self.handle_other_inputs_connected(ws_write, other).await,
        }
        None
    }

    async fn handle_other_inputs_connected(
        &mut self,
        ws_write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
        input: RelayInput,
    ) {
        let result = match input {
            RelayInput::EoseActions(action_id, subscriptions) => {
                if let Some(sub) = subscriptions.first() {
                    match self.subscribe(sub.to_owned(), ws_write).await {
                        Ok(_) => {
                            let mut actions = BTreeMap::new();
                            for subscription in subscriptions {
                                actions.insert(subscription.id.clone(), subscription);
                            }
                            self.eose_actions.insert(action_id, actions);
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    Ok(())
                }
            }
            RelayInput::FetchInformation => {
                let information = self.information.clone();
                _ = self
                    .notification_tx
                    .send(RelayEvent::RelayInformation(information).into(&self.url));
                Ok(())
            }
            RelayInput::ToggleRead(read) => {
                self.read = read;
                Ok(())
            }
            RelayInput::ToggleWrite(write) => {
                self.write = write;
                Ok(())
            }
            RelayInput::CloseSubscription(sub_id) => {
                self.close_subscription(&sub_id, ws_write).await
            }
            RelayInput::SendEvent(event) => self.send_event(event, ws_write).await,
            RelayInput::Subscribe(subscription) => self.subscribe(subscription, ws_write).await,
            RelayInput::Count(subscription) => self.count(subscription, ws_write).await,
            RelayInput::Close => {
                unreachable!("RelayInput::Close should not be handled by this function");
            }
            RelayInput::Reconnect => {
                // Only reconnects when terminated
                log::debug!("Ignoring reconnect request when connected");
                Ok(())
            }
        };

        if let Err(e) = result {
            log::debug!("error sendind count: {}", e);
            _ = self
                .notification_tx
                .send(RelayEvent::SendError(e).into(&self.url));
        }
    }

    async fn handle_connected(&mut self, mut state: RelayState) -> (LoopControl, RelayState) {
        if let RelayState::Connected { ws_read, ws_write } = &mut state {
            // Handle input queue
            while let Some(input) = self.input_queue.pop_front() {
                if let Some((control, state)) = self.handle_input_connected(ws_write, input).await {
                    return (control, state);
                }
            }

            if let DocRequestState::Initial = self.document_request {
                let (doc_tx, doc_rx) = oneshot::channel();
                spawn_get_document(self.url.to_owned(), doc_tx);
                self.document_request = DocRequestState::Requested(doc_rx);
                return (LoopControl::Continue, state);
            }

            // Create future depending on whether relay.read is true or false
            let ws_fut = if self.read {
                Either::Left(ws_read.next())
            } else {
                log::debug!("{}", Error::BlockedReadFromRelay(self.url.to_owned()));
                Either::Right(pending())
            };

            // Receive subscription response
            tokio::select! {
                doc_result = self.document_request.get_fut() => {
                    match doc_result {
                        Ok(Ok(document)) => {
                            self.set_document(&document);
                        }
                        Ok(Err(e)) => {
                            // failed to fetch document
                            log::debug!("{} - doc err: {}", &self.url, e);
                            self.document_request = DocRequestState::Failed;
                        }
                        Err(e) => {
                            log::debug!("{} - doc err: {}", &self.url, e);
                        }
                    }
                }
                message = ws_fut => {
                    match message {
                        Some(Ok(msg)) if msg.is_text() => {
                            // Handle incoming messages here
                            match msg.to_text() {
                                Ok(msg_text) => match RelayMessage::from_json(msg_text) {
                                    Ok(message) => {
                                        if let Err(e) = self.check_eose(&message, ws_write).await{
                                            _ = self.notification_tx.send(RelayEvent::SendError(e).into(&self.url));
                                        };
                                        self.check_ok_msg(&message).await;
                                        let _  = self.notification_tx.send(RelayEvent::RelayMessage(message).into(&self.url));
                                    },
                                    Err(e) => {
                                        log::debug!("Error parsing message from {}: {}", &self.url, e);
                                    }
                                },
                                Err(e) => {
                                    log::debug!("Error parsing message from {}: {}", &self.url, e);
                                }
                            }
                        }
                        Some(Ok(msg)) if msg.is_close() => {
                            _ = self.pool_tx.send(RelayToPoolEvent::RelayDisconnected.into(&self.url)).await;
                            self.terminated();
                            return (LoopControl::Continue, RelayState::Terminated);
                        }
                        Some(Err(e)) => {
                            log::debug!("Error reading message from {}: {}", &self.url, e);
                        }
                        _ => (),
                    }
                }
                input = self.input.recv() => {
                    if let Some(input) = input {
                        if let Some((control, state)) = self.handle_input_connected(ws_write, input).await {
                            return (control, state);
                        }
                    } else {
                        log::debug!("{} - relay input receiver closed", &self.url);
                        return (LoopControl::Break, state);
                    }
                }
                timeout = self.timeout_rx.recv() => {
                    if let Some(sub_id) = timeout {
                        if self.subscriptions.get(&sub_id).is_some() {
                            log::debug!("{}: Timeout for subscription {}", &self.url, &sub_id);

                            if let Err(e) = self.close_subscription(&sub_id, ws_write).await{
                                _ = self.notification_tx.send(RelayEvent::SendError(e).into(&self.url));
                            }

                            _ = self.notification_tx.send(RelayEvent::Timeout(sub_id).into(&self.url));
                        }
                    } else {
                        log::debug!("{} - relay timeout receiver closed", &self.url);
                        return (LoopControl::Break, state);
                    }
                }
            }
        }
        (LoopControl::Continue, state)
    }

    pub async fn run(&mut self) {
        let mut state = RelayState::new();
        loop {
            let (control, new_state) = match state {
                RelayState::Terminated => self.handle_terminated(state).await,
                RelayState::Disconnected { .. } => self.handle_disconnected(state).await,
                RelayState::Connecting { .. } => self.handle_connecting(state).await,
                RelayState::Connected { .. } => self.handle_connected(state).await,
            };

            state = new_state;

            match control {
                LoopControl::Break => break,
                LoopControl::Continue => (),
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

pub enum LoopControl {
    Break,
    Continue,
}

pub enum RelayState {
    Connected {
        ws_read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        ws_write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    },
    Connecting {
        conn_rx: oneshot::Receiver<ConnResult>,
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
    FetchInformation,
    EoseActions(String, Vec<Subscription>),
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
impl RelayToPoolEvent {
    pub fn into(self, url: &Url) -> RelayToPool {
        RelayToPool {
            url: url.to_owned(),
            event: self,
        }
    }
}
#[derive(Debug, Clone)]
pub enum RelayEvent {
    Timeout(SubscriptionId),
    RelayMessage(RelayMessage),
    SentSubscription(SubscriptionId),
    SentEvent(nostr::EventId),
    RelayInformation(RelayInformation),
    SentCount(SubscriptionId),
    SendError(SendError),
    ActionsDone(String),
}
impl RelayEvent {
    pub fn into(self: Self, url: &Url) -> NotificationEvent {
        NotificationEvent {
            url: url.to_owned(),
            event: self,
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
    pub fn new(filters: Vec<Filter>) -> Self {
        Self {
            id: SubscriptionId::generate(),
            filters,
            sub_type: SubscriptionType::Endless,
            status: SubscriptionStatus::Active,
            timeout: None,
        }
    }
    pub fn eose(filters: Vec<Filter>) -> Self {
        let mut subscription = Self::new(filters);
        subscription.sub_type = SubscriptionType::UntilEOSE;
        subscription
    }
    pub fn count(filters: Vec<Filter>) -> Self {
        let mut subscription = Self::new(filters);
        subscription.sub_type = SubscriptionType::Count;
        subscription
    }
    pub fn action(filters: Vec<Filter>) -> Self {
        let mut subscription = Self::new(filters);
        subscription.sub_type = SubscriptionType::Action;
        subscription
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
    Action,
}
#[derive(Debug, Clone)]
pub enum SubscriptionStatus {
    Active,
    Done,
}

const MAX_ERROR_MSGS_LIMIT: u8 = 100;

// match doc_rx.await {
//     Ok(Ok(document)) => {
//         relay.set_document(&document);
//         // Some(RelayEvent::RelayNotification(
//         //     RelayNotificationEvent::RelayDocument(Some(document)),
//         // ))
//     }
//     Ok(Err(e)) => {
//         log::info!("{} - doc err: {}", &relay.url, e);
//     }
//     Err(e) => {
//         log::trace!("{} - doc_rx closed", &relay.url);
//     }
// }

// if self.document.is_none() && !self.is_document_requested {
//     let (doc_tx, doc_rx) = oneshot::channel();

//     spawn_get_document(self.url.to_owned(), doc_tx);

//     match doc_rx.await {
//         Ok(Ok(document)) => {
//             self.set_document(&document);
//             _ = self.notification_tx.send(
//                 RelayEvent::RelayDocument(Some(document)).into(&self.url),
//             );
//         }
//         Ok(Err(e)) => {
//             // failed to fetch document
//             log::debug!("{} - doc err: {}", &self.url, e);
//         }
//         Err(e) => {
//             log::debug!("{} - doc err: {}", &self.url, e);
//         }
//     }
//     self.is_document_requested = true;
// }
