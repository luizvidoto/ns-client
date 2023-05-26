use nostr::Filter;
use nostr::RelayMessage;
use nostr::SubscriptionId;
use std::collections::HashMap;
use tokio::sync::{broadcast, mpsc};
use url::Url;

use crate::relay::spawn_relay_task;
use crate::relay::RelayInput;
use crate::relay::RelayOptions;
use crate::relay::RelayToPoolEvent;
use crate::Error;
use crate::RelayStatus;

#[derive(Debug, Clone)]
pub struct RelayListener {
    relay_tx: mpsc::Sender<RelayInput>,
    status: RelayStatus,
}
impl RelayListener {
    pub fn new(relay_tx: mpsc::Sender<RelayInput>) -> Self {
        Self {
            relay_tx,
            status: RelayStatus::default(),
        }
    }
    pub fn update_status(&mut self, status: RelayStatus) {
        self.status = status;
    }
}

pub struct RelayPoolTask {
    outside_rx: mpsc::UnboundedReceiver<PoolInput>,
    notification_sender: broadcast::Sender<NotificationEvent>,
    to_pool_tx: mpsc::Sender<RelayToPool>,
    to_pool_rx: mpsc::Receiver<RelayToPool>,
    relays: HashMap<Url, RelayListener>,
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
    fn handle_relay_to_pool(&mut self, msg: RelayToPool) -> Option<NotificationEvent> {
        match msg.event {
            RelayToPoolEvent::ConnectedToSocket => {
                log::info!("{} - Connected to socket", &msg.url);

                if let Some(listener) = self.relays.get_mut(&msg.url) {
                    listener.update_status(RelayStatus::Connected);
                    for (sub_id, filters) in &self.subscriptions {
                        let relay_tx_1 = listener.relay_tx.clone();
                        let sub_id_1 = sub_id.clone();
                        let filters_1 = filters.clone();
                        tokio::spawn(async move {
                            if let Err(e) = relay_tx_1
                                .send(RelayInput::Subscribe(sub_id_1, filters_1))
                                .await
                            {
                                log::debug!("relay dropped: {}", e);
                            }
                        });
                    }
                }

                None
            }
            RelayToPoolEvent::Reconnecting => {
                if let Some(listener) = self.relays.get_mut(&msg.url) {
                    listener.update_status(RelayStatus::Connecting);
                }
                None
            }
            RelayToPoolEvent::FailedToConnect => {
                if let Some(listener) = self.relays.get_mut(&msg.url) {
                    listener.update_status(RelayStatus::Disconnected);
                }
                None
            }
            RelayToPoolEvent::RelayDisconnected => {
                if let Some(listener) = self.relays.get_mut(&msg.url) {
                    listener.update_status(RelayStatus::Disconnected);
                }
                None
            }
            RelayToPoolEvent::AttempToConnect => {
                if let Some(listener) = self.relays.get_mut(&msg.url) {
                    listener.update_status(RelayStatus::Connecting);
                }
                None
            }
            RelayToPoolEvent::MaxAttempsExceeded => {
                if let Some(listener) = self.relays.get_mut(&msg.url) {
                    listener.update_status(RelayStatus::Terminated);
                }
                Some(NotificationEvent::RelayTerminated(msg.url.clone()))
            }
            RelayToPoolEvent::TerminateRelay => {
                if let Some(listener) = self.relays.get_mut(&msg.url) {
                    listener.update_status(RelayStatus::Terminated);
                }
                Some(NotificationEvent::RelayTerminated(msg.url.clone()))
            }
            RelayToPoolEvent::Closed => {
                if let Some(listener) = self.relays.get_mut(&msg.url) {
                    listener.update_status(RelayStatus::Terminated);
                }
                None
            }
        }
    }
    pub async fn run(&mut self) {
        loop {
            let event_opt = tokio::select! {
                relay_input = self.to_pool_rx.recv() => {
                    if let Some(msg) = relay_input {
                        self.handle_relay_to_pool(msg)
                    } else {
                        log::debug!("Relay pool task channel closed");
                        None
                    }
                }
                pool_input = self.outside_rx.recv() => {
                    if let Some(input) = pool_input {
                        match input {
                            PoolInput::SubscribeEOSE(_,_) => {
                                todo!()
                            }
                            PoolInput::Shutdown => {
                                for (_url, listener) in &mut self.relays{
                                    if let Err(e) = listener.relay_tx.send(RelayInput::Close).await {
                                        log::debug!("Failed to send close to relay: {}", e);
                                    }
                                }
                                break;
                            }
                            PoolInput::AddRelay(url, _opts) => {
                                // TODO: use opts
                                process_add_relay(&self.to_pool_tx, &self.notification_sender, url, &mut self.relays).await;
                                None
                            }
                            PoolInput::SendEvent(event) => {
                                for (_url, listener) in &mut self.relays{
                                    if let Err(e) = listener.relay_tx.send(RelayInput::SendEvent(event.clone())).await {
                                        log::debug!("Failed to send event to relay: {}", e);
                                    }
                                }
                                None
                            }
                            PoolInput::ReconnectRelay(url) => {
                                process_reconnect_relay(url, &mut self.relays).await;
                                None
                            }
                            PoolInput::AddSubscription(sub_id, filters) => {
                                // process_subscribe_all(sub_id, filters, &mut self.relays).await
                                self.subscriptions.insert(sub_id.clone(), filters);
                                None
                            }
                            PoolInput::RemoveRelay(url) => {
                                if let Some(listener) = &mut self.relays.get_mut(&url) {
                                    if let Err(e) = listener.relay_tx.send(RelayInput::Close).await {
                                        log::debug!("Failed to send event to relay: {}", e);
                                    }
                                }
                                self.relays.remove(&url);
                                None
                            }
                            PoolInput::ToggleReadFor(_url, _read) => {
                                todo!()
                            }
                            PoolInput::ToggleWriteFor(_url, _write) => {
                                todo!()
                            }
                            PoolInput::GetRelayStatusList(one_tx) => {
                                let status:RelayStatusList = self.relays.iter().map(|(url, listener)| (url.clone(), listener.status)).collect();
                                if let Err(e) = one_tx.send(status).await {
                                    log::debug!("Failed to send relays status: {}", e);
                                }
                                None
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

#[derive(Debug, Clone)]
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
        let msg = PoolInput::AddSubscription(sub_id.clone(), filters);
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(sub_id)
    }
    pub fn subscribe_relay(&self, _url: &Url, _filters: Vec<Filter>) -> Result<(), Error> {
        // let sub_id = SubscriptionId::generate();
        // let msg = PoolInput::AddSubscription(sub_id.clone(), filters);
        // self.outside_tx
        //     .send(msg.clone())
        //     .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        // Ok(sub_id)
        todo!()
    }

    /// Subscribe until relay sends an "End Of Stored Events" event
    pub fn subscribe_eose(&self, id: SubscriptionId, filters: Vec<Filter>) -> Result<(), Error> {
        let msg = PoolInput::SubscribeEOSE(id, filters);
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(())
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
        let msg = PoolInput::AddRelay(url.clone(), opts);
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(url)
    }
    pub fn add_relay_with_opts(&self, url: &str, opts: RelayOptions) -> Result<Url, Error> {
        log::trace!("Adding relay {}", url);
        let url = Url::parse(url)?;
        let msg = PoolInput::AddRelay(url.clone(), opts);
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
        let msg = PoolInput::ToggleReadFor(url.to_owned(), read);
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(())
    }
    pub fn toggle_write_for(&self, url: &Url, write: bool) -> Result<(), Error> {
        log::info!("toggle write for {} - write: {}", url, write);
        let msg = PoolInput::ToggleWriteFor(url.to_owned(), write);
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(())
    }
    pub async fn relay_status_list(&self) -> Result<RelayStatusList, Error> {
        log::info!("relays status list");
        let (tx, mut rx) = mpsc::channel(1);
        let msg = PoolInput::GetRelayStatusList(tx);

        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;

        if let Some(list) = rx.recv().await {
            Ok(list)
        } else {
            Err(Error::UnableToGetRelaysStatus)
        }
    }
}

pub type RelayStatusList = Vec<(Url, RelayStatus)>;

#[derive(Debug, Clone)]
pub enum PoolInput {
    AddRelay(Url, RelayOptions),
    ReconnectRelay(Url),
    AddSubscription(SubscriptionId, Vec<Filter>),
    Shutdown,
    SendEvent(nostr::Event),
    RemoveRelay(Url),
    ToggleReadFor(Url, bool),
    ToggleWriteFor(Url, bool),
    GetRelayStatusList(mpsc::Sender<RelayStatusList>),
    SubscribeEOSE(nostr::SubscriptionId, Vec<Filter>),
}
#[derive(Debug, Clone)]
pub enum NotificationEvent {
    // RelayAdded(Url),
    // RelayConnected(Url),
    // RelayConnecting(Url),
    // RelayDisconnected(Url),
    RelayTerminated(Url),
    RelayMessage(Url, RelayMessage),
    SentSubscription(Url, SubscriptionId),
    SentEvent(Url, nostr::EventId),
}
#[derive(Debug, Clone)]
pub struct RelayToPool {
    pub url: Url,
    pub event: RelayToPoolEvent,
}

async fn process_add_relay(
    to_pool_sender: &mpsc::Sender<RelayToPool>,
    pool_event_sender: &broadcast::Sender<NotificationEvent>,
    url: Url,
    relays: &mut HashMap<Url, RelayListener>,
) {
    let (relay_input_sender, relay_input_receiver) = mpsc::channel(RELAY_INPUT_CHANNEL_SIZE);
    let relay_task = spawn_relay_task(
        url.clone(),
        to_pool_sender.clone(),
        relay_input_sender.clone(),
        relay_input_receiver,
        pool_event_sender.clone(),
    );
    tokio::spawn(relay_task);
    relays.insert(url.clone(), RelayListener::new(relay_input_sender));
}

async fn process_reconnect_relay(url: Url, relays: &mut HashMap<Url, RelayListener>) {
    if let Some(listener) = relays.get_mut(&url) {
        if let Err(e) = listener.relay_tx.try_send(RelayInput::Reconnect) {
            log::debug!("Failed to send reconnect to relay: {}", e);
        }
    } else {
        log::warn!("Relay not found");
    }
}

const EVENT_SENDER_CHANNEL_SIZE: usize = 10000;
const RELAY_TO_POOL_CHANNEL_SIZE: usize = 10;
const RELAY_INPUT_CHANNEL_SIZE: usize = 10;
