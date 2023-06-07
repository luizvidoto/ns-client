use nostr::Filter;
use nostr::SubscriptionId;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use url::Url;

use crate::relay::Relay;
use crate::relay::RelayEvent;
use crate::relay::RelayInput;
use crate::relay::RelayOptions;
use crate::relay::RelayToPoolEvent;
use crate::relay::Subscription;
use crate::relay::SubscriptionType;
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
            status: RelayStatus::Disconnected,
        }
    }
    pub fn set_status(&mut self, status: RelayStatus) {
        self.status = status;
    }
}

pub struct RelayPoolTask {
    outside_rx: mpsc::UnboundedReceiver<PoolInput>,
    notification_sender: broadcast::Sender<NotificationEvent>,
    to_pool_tx: mpsc::Sender<RelayToPool>,
    to_pool_rx: mpsc::Receiver<RelayToPool>,
    relays: HashMap<Url, RelayListener>,
    subscriptions: HashMap<SubscriptionId, Subscription>,
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
                    listener.set_status(RelayStatus::Connected);
                    for (_sub_id, subscription) in &self.subscriptions {
                        let relay_tx_1 = listener.relay_tx.clone();
                        let subscription = subscription.to_owned();
                        let message = match subscription.sub_type {
                            SubscriptionType::Count => RelayInput::Count(subscription),
                            SubscriptionType::UntilEOSE => RelayInput::Subscribe(subscription),
                            SubscriptionType::Endless => RelayInput::Subscribe(subscription),
                            SubscriptionType::Action => RelayInput::Subscribe(subscription),
                        };
                        tokio::spawn(async move {
                            if let Err(e) = relay_tx_1.send(message).await {
                                log::debug!("relay dropped: {}", e);
                            }
                        });
                    }
                }

                None
            }
            RelayToPoolEvent::Reconnecting => {
                if let Some(listener) = self.relays.get_mut(&msg.url) {
                    listener.set_status(RelayStatus::Disconnected);
                }
                None
            }
            RelayToPoolEvent::AttempToConnect => {
                if let Some(listener) = self.relays.get_mut(&msg.url) {
                    listener.set_status(RelayStatus::Connecting);
                }
                None
            }
            RelayToPoolEvent::FailedToConnect => {
                if let Some(listener) = self.relays.get_mut(&msg.url) {
                    listener.set_status(RelayStatus::Disconnected);
                }
                None
            }
            RelayToPoolEvent::RelayDisconnected => {
                if let Some(listener) = self.relays.get_mut(&msg.url) {
                    listener.set_status(RelayStatus::Disconnected);
                }
                None
            }

            RelayToPoolEvent::MaxAttempsExceeded => {
                if let Some(listener) = self.relays.get_mut(&msg.url) {
                    listener.set_status(RelayStatus::Terminated);
                }
                // Some(NotificationEvent::RelayTerminated(msg.url.clone()))
                None
            }
            RelayToPoolEvent::TerminateRelay => {
                if let Some(listener) = self.relays.get_mut(&msg.url) {
                    listener.set_status(RelayStatus::Terminated);
                }
                // Some(NotificationEvent::RelayTerminated(msg.url.clone()))
                None
            }
            RelayToPoolEvent::Closed => {
                if let Some(listener) = self.relays.get_mut(&msg.url) {
                    listener.set_status(RelayStatus::Terminated);
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
                            PoolInput::Shutdown => {
                                for (_url, listener) in &mut self.relays {
                                    if let Err(e) = listener.relay_tx.send(RelayInput::Close).await {
                                        log::debug!("Failed to send Close to relay: {}", e);
                                    }
                                }
                                break;
                            },
                            other => self.handle_input(other).await
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

    async fn send_to_listener(&mut self, url: &Url, input: RelayInput) {
        if let Some(listener) = self.relays.get_mut(&url) {
            if let Err(e) = listener.relay_tx.send(input).await {
                log::debug!("Failed to send to relay: {}", e);
            }
        }
    }

    async fn send_to_all(&mut self, input: RelayInput) {
        for (url, listener) in &mut self.relays {
            if let Err(e) = listener.relay_tx.send(input.clone()).await {
                log::debug!("Failed to send to relay: {} - {}", url, e);
            }
        }
    }

    async fn handle_input(&mut self, input: PoolInput) -> Option<NotificationEvent> {
        match input {
            PoolInput::EoseActionsTo(url, actions_id, subscriptions) => {
                log::debug!("Eose actions to {} - {:?}", url, subscriptions);
                self.send_to_listener(&url, RelayInput::EoseActions(actions_id, subscriptions))
                    .await;
                None
            }
            PoolInput::Shutdown => {
                unreachable!("Shutdown should be handled before")
            }
            PoolInput::GetRelayInformation(url) => {
                self.send_to_listener(&url, RelayInput::FetchInformation)
                    .await;
                None
            }
            PoolInput::AddRelay(url, opts) => {
                process_add_relay(
                    url,
                    opts,
                    self.to_pool_tx.clone(),
                    self.notification_sender.clone(),
                    &mut self.relays,
                )
                .await;
                None
            }
            PoolInput::SendEvent(event) => {
                self.send_to_all(RelayInput::SendEvent(event)).await;
                None
            }
            PoolInput::ReconnectRelay(url) => {
                process_reconnect_relay(url, &mut self.relays).await;
                None
            }
            PoolInput::Count(subscription) => {
                self.subscriptions
                    .insert(subscription.id.clone(), subscription.clone());
                self.send_to_all(RelayInput::Count(subscription)).await;
                None
            }
            PoolInput::AddSubscription(subscription) => {
                self.subscriptions
                    .insert(subscription.id.clone(), subscription.clone());
                self.send_to_all(RelayInput::Subscribe(subscription)).await;
                None
            }
            PoolInput::AddSubscriptionToRelay(relay_url, subscription) => {
                self.subscriptions
                    .insert(subscription.id.clone(), subscription.clone());
                self.send_to_listener(&relay_url, RelayInput::Subscribe(subscription))
                    .await;
                None
            }
            PoolInput::RemoveRelay(url) => {
                self.send_to_listener(&url, RelayInput::Close).await;
                self.relays.remove(&url);
                None
            }
            PoolInput::ToggleReadFor(url, read) => {
                self.send_to_listener(&url, RelayInput::ToggleRead(read))
                    .await;
                None
            }
            PoolInput::ToggleWriteFor(url, write) => {
                self.send_to_listener(&url, RelayInput::ToggleWrite(write))
                    .await;
                None
            }
            PoolInput::GetAllRelaysInformation => {
                self.send_to_all(RelayInput::FetchInformation).await;
                None
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
    pub fn subscribe(
        &self,
        filters: Vec<Filter>,
        timeout: Option<Duration>,
    ) -> Result<SubscriptionId, Error> {
        let subscription = Subscription::new(filters).timeout(timeout);
        let sub_id = subscription.id.clone();
        let msg = PoolInput::AddSubscription(subscription);
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(sub_id)
    }
    pub fn subscribe_id(
        &self,
        id: &SubscriptionId,
        filters: Vec<Filter>,
        timeout: Option<Duration>,
    ) -> Result<(), Error> {
        let subscription = Subscription::new(filters).with_id(id).timeout(timeout);
        let msg = PoolInput::AddSubscription(subscription);
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(())
    }
    /// Subscribe until relay sends an "End Of Stored Events" notification
    pub fn subscribe_eose(
        &self,
        id: &SubscriptionId,
        filters: Vec<Filter>,
        timeout: Option<Duration>,
    ) -> Result<(), Error> {
        let subscription = Subscription::eose(filters).with_id(id).timeout(timeout);
        let msg = PoolInput::AddSubscription(subscription);
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(())
    }
    pub fn count(
        &self,
        id: &SubscriptionId,
        filters: Vec<Filter>,
        timeout: Option<Duration>,
    ) -> Result<(), Error> {
        let subscription = Subscription::count(filters).with_id(id).timeout(timeout);
        let msg = PoolInput::Count(subscription);
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(())
    }
    pub fn subscribe_relay(
        &self,
        url: &Url,
        filters: Vec<Filter>,
        timeout: Option<Duration>,
    ) -> Result<SubscriptionId, Error> {
        let subscription = Subscription::new(filters).timeout(timeout);
        let sub_id = subscription.id.clone();
        let msg = PoolInput::AddSubscriptionToRelay(url.to_owned(), subscription);
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(sub_id)
    }
    pub fn relay_subscribe_eose(
        &self,
        url: &Url,
        id: &SubscriptionId,
        filters: Vec<Filter>,
        timeout: Option<Duration>,
    ) -> Result<(), Error> {
        let subscription = Subscription::eose(filters).with_id(id).timeout(timeout);
        let msg = PoolInput::AddSubscriptionToRelay(url.to_owned(), subscription);
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

    pub fn relays_info(&self) -> Result<(), Error> {
        log::info!("relays information list");
        let msg = PoolInput::GetAllRelaysInformation;
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(())
    }
    pub fn relay_info(&self, url: &Url) -> Result<(), Error> {
        log::info!("relay info {}", url);
        let msg = PoolInput::GetRelayInformation(url.to_owned());
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(())
    }
    pub fn relay_eose_actions(
        &self,
        url: &Url,
        actions_id: &str,
        actions: Vec<Subscription>,
    ) -> Result<(), Error> {
        let msg = PoolInput::EoseActionsTo(url.to_owned(), actions_id.to_owned(), actions);
        self.outside_tx
            .send(msg.clone())
            .map_err(|e| Error::SendToPoolTaskFailed(e.to_string(), msg.clone()))?;
        Ok(())
    }
}

pub type RelayStatusList = Vec<(Url, RelayStatus)>;

#[derive(Debug, Clone)]
pub enum PoolInput {
    AddRelay(Url, RelayOptions),
    ReconnectRelay(Url),
    AddSubscription(Subscription),
    Shutdown,
    SendEvent(nostr::Event),
    RemoveRelay(Url),
    ToggleReadFor(Url, bool),
    ToggleWriteFor(Url, bool),
    AddSubscriptionToRelay(Url, Subscription),
    Count(Subscription),
    GetRelayInformation(Url),
    GetAllRelaysInformation,
    EoseActionsTo(Url, String, Vec<Subscription>),
}
#[derive(Debug, Clone)]
pub struct RelayToPool {
    pub url: Url,
    pub event: RelayToPoolEvent,
}
#[derive(Debug, Clone)]
pub struct NotificationEvent {
    pub url: Url,
    pub event: RelayEvent,
}

async fn process_add_relay(
    url: Url,
    opts: RelayOptions,
    pool_tx: mpsc::Sender<RelayToPool>,
    notification_tx: broadcast::Sender<NotificationEvent>,
    listeners: &mut HashMap<Url, RelayListener>,
) {
    let (relay_input_sender, relay_input_receiver) = mpsc::channel(RELAY_INPUT_CHANNEL_SIZE);
    let mut relay = Relay::new(&url, relay_input_receiver, pool_tx, notification_tx, opts);
    tokio::spawn(async move {
        relay.run().await;
    });
    listeners.insert(url.clone(), RelayListener::new(relay_input_sender));
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
const RELAY_TO_POOL_CHANNEL_SIZE: usize = 100;
const RELAY_INPUT_CHANNEL_SIZE: usize = 100;
