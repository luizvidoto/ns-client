use nostr::{Event, SubscriptionId};
use std::collections::HashMap;
use tokio::sync::oneshot;
use tokio::sync::{broadcast, mpsc};
use url::Url;

use crate::relay::Relay;
use crate::relay::RelayEvent;
use crate::relay::RelayInput;
use crate::relay::RelayOptions;
use crate::relay::RelayToPoolEvent;
use crate::relay::Subscription;
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
                        tokio::spawn(async move {
                            if let Err(e) =
                                relay_tx_1.send(RelayInput::Subscribe(subscription)).await
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
                            other => {
                                let break_loop = self.handle_input(other).await;
                                if break_loop {
                                    break;
                                }
                            }
                        }
                    } else {
                        log::debug!("Outside channel closed");
                        break;
                    }
                    None
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
            // maybe another task here?
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

    async fn handle_input(&mut self, input: PoolInput) -> bool {
        match input {
            PoolInput::Auth(url, event) => {
                log::debug!("Auth to {}", url);
                self.send_to_listener(&url, RelayInput::Auth(event)).await;
            }
            PoolInput::EoseActionsTo(url, actions_id, subscriptions) => {
                log::debug!("Eose actions to {} - {:?}", url, subscriptions);
                self.send_to_listener(&url, RelayInput::EoseActions(actions_id, subscriptions))
                    .await;
            }
            PoolInput::Shutdown => {
                unreachable!("Shutdown should be handled before")
            }
            PoolInput::GetRelayInformation(url) => {
                self.send_to_listener(&url, RelayInput::FetchInformation)
                    .await;
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
            }
            PoolInput::SendEvent(event) => {
                self.send_to_all(RelayInput::SendEvent(event)).await;
            }
            PoolInput::ReconnectRelay(url) => {
                self.send_to_listener(&url, RelayInput::Reconnect).await;
            }
            PoolInput::AddSubscription(subscription) => {
                self.subscriptions
                    .insert(subscription.id.clone(), subscription.clone());
                self.send_to_all(RelayInput::Subscribe(subscription)).await;
            }
            PoolInput::AddSubscriptionToRelay(relay_url, subscription) => {
                self.subscriptions
                    .insert(subscription.id.clone(), subscription.clone());
                self.send_to_listener(&relay_url, RelayInput::Subscribe(subscription))
                    .await;
            }
            PoolInput::RemoveRelay(url) => {
                self.send_to_listener(&url, RelayInput::Close).await;
                self.relays.remove(&url);
            }
            PoolInput::ToggleReadFor(url, read) => {
                self.send_to_listener(&url, RelayInput::ToggleRead(read))
                    .await;
            }
            PoolInput::ToggleWriteFor(url, write) => {
                self.send_to_listener(&url, RelayInput::ToggleWrite(write))
                    .await;
            }
            PoolInput::GetAllRelaysInformation => {
                self.send_to_all(RelayInput::FetchInformation).await;
            }
            PoolInput::GetRelayStatusList(one_tx) => {
                let status: RelayStatusList = self
                    .relays
                    .iter()
                    .map(|(url, listener)| (url.clone(), listener.status))
                    .collect();

                if let Err(_e) = one_tx.send(status) {
                    log::debug!("Failed to send relays status");
                    return true;
                }
            }
        }

        false
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
    pub fn subscribe(&self, subscription: &Subscription) -> Result<(), Error> {
        let msg = PoolInput::AddSubscription(subscription.to_owned());
        self.outside_tx
            .send(msg)
            .map_err(|e| Error::SendToPoolTaskFailed(format!("AddSubscription. error: {}", e)))?;
        Ok(())
    }
    pub fn relay_subscribe(&self, url: &Url, subscription: &Subscription) -> Result<(), Error> {
        let msg = PoolInput::AddSubscriptionToRelay(url.to_owned(), subscription.to_owned());
        self.outside_tx.send(msg).map_err(|e| {
            Error::SendToPoolTaskFailed(format!("AddSubscriptionToRelay. error: {}", e))
        })?;
        Ok(())
    }

    pub fn reconnect_relay(&self, url: &Url) -> Result<(), Error> {
        let msg = PoolInput::ReconnectRelay(url.to_owned());
        self.outside_tx
            .send(msg)
            .map_err(|e| Error::SendToPoolTaskFailed(format!("ReconnectRelay. error: {}", e)))?;
        Ok(())
    }
    pub fn add_relay(&self, url: &str) -> Result<Url, Error> {
        log::trace!("Adding relay {}", url);
        let url = Url::parse(url)?;
        let opts = RelayOptions::default();
        let msg = PoolInput::AddRelay(url.clone(), opts);
        self.outside_tx
            .send(msg)
            .map_err(|e| Error::SendToPoolTaskFailed(format!("AddRelay. error: {}", e)))?;
        Ok(url)
    }
    pub fn add_relay_with_opts(&self, url: &str, opts: RelayOptions) -> Result<Url, Error> {
        log::trace!("Adding relay {}", url);
        let url = Url::parse(url)?;
        let msg = PoolInput::AddRelay(url.clone(), opts);
        self.outside_tx
            .send(msg)
            .map_err(|e| Error::SendToPoolTaskFailed(format!("AddRelay. error: {}", e)))?;
        Ok(url)
    }
    pub fn shutdown(&self) -> Result<(), Error> {
        log::info!("shutdown client");
        let msg = PoolInput::Shutdown;
        self.outside_tx
            .send(msg)
            .map_err(|e| Error::SendToPoolTaskFailed(format!("Shutdown. error: {}", e)))?;
        Ok(())
    }
    pub fn send_event(&self, event: nostr::Event) -> Result<(), Error> {
        log::trace!("send event to relays");
        let msg = PoolInput::SendEvent(event);
        self.outside_tx
            .send(msg)
            .map_err(|e| Error::SendToPoolTaskFailed(format!("SendEvent. error: {}", e)))?;
        Ok(())
    }
    pub fn remove_relay(&self, url: &str) -> Result<(), Error> {
        log::trace!("remove relay {}", url);
        let url = Url::parse(url)?;
        let msg = PoolInput::RemoveRelay(url);
        self.outside_tx
            .send(msg)
            .map_err(|e| Error::SendToPoolTaskFailed(format!("RemoveRelay. error: {}", e)))?;
        Ok(())
    }
    pub fn toggle_read_for(&self, url: &Url, read: bool) -> Result<(), Error> {
        log::trace!("toggle write for {} - read: {}", url, read);
        let msg = PoolInput::ToggleReadFor(url.to_owned(), read);
        self.outside_tx
            .send(msg)
            .map_err(|e| Error::SendToPoolTaskFailed(format!("ToggleReadFor. error: {}", e)))?;
        Ok(())
    }
    pub fn toggle_write_for(&self, url: &Url, write: bool) -> Result<(), Error> {
        log::trace!("toggle write for {} - write: {}", url, write);
        let msg = PoolInput::ToggleWriteFor(url.to_owned(), write);
        self.outside_tx
            .send(msg)
            .map_err(|e| Error::SendToPoolTaskFailed(format!("ToggleWriteFor. error: {}", e)))?;
        Ok(())
    }

    pub fn relays_info(&self) -> Result<(), Error> {
        log::trace!("relays information list");
        let msg = PoolInput::GetAllRelaysInformation;
        self.outside_tx.send(msg).map_err(|e| {
            Error::SendToPoolTaskFailed(format!("GetAllRelaysInformation. error: {}", e))
        })?;
        Ok(())
    }
    pub fn relay_info(&self, url: &Url) -> Result<(), Error> {
        log::trace!("relay info {}", url);
        let msg = PoolInput::GetRelayInformation(url.to_owned());
        self.outside_tx.send(msg).map_err(|e| {
            Error::SendToPoolTaskFailed(format!("GetRelayInformation. error: {}", e))
        })?;
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
            .send(msg)
            .map_err(|e| Error::SendToPoolTaskFailed(format!("EoseActionsTo. error: {}", e)))?;
        Ok(())
    }
    pub async fn relay_status_list(&self) -> Result<RelayStatusList, Error> {
        log::trace!("relays status list");
        let (tx, rx) = oneshot::channel();
        let msg = PoolInput::GetRelayStatusList(tx);

        self.outside_tx.send(msg).map_err(|e| {
            Error::SendToPoolTaskFailed(format!("GetRelayStatusList. error: {}", e))
        })?;

        // tokio::select! {
        //     _ = tokio::time::sleep(Duration::from_secs(3)) => (),
        //     list_result = rx => {
        //         if let Ok(list) = list_result {
        //             return Ok(list)
        //         }
        //     }
        // }

        if let Ok(list) = rx.await {
            return Ok(list);
        }

        Err(Error::UnableToGetRelaysStatus)
    }
    /// Send auth event to selected relay. [NIP-42](https://github.com/nostr-protocol/nips/blob/master/42.md)
    pub fn send_auth(&self, relay_url: &Url, auth_event: Event) -> Result<(), Error> {
        let msg = PoolInput::Auth(relay_url.to_owned(), auth_event);
        self.outside_tx
            .send(msg)
            .map_err(|e| Error::SendToPoolTaskFailed(format!("EoseActionsTo. error: {}", e)))?;
        Ok(())
    }
}

pub type RelayStatusList = Vec<(Url, RelayStatus)>;

#[derive(Debug)]
pub enum PoolInput {
    AddRelay(Url, RelayOptions),
    ReconnectRelay(Url),
    AddSubscription(Subscription),
    Shutdown,
    SendEvent(Event),
    RemoveRelay(Url),
    ToggleReadFor(Url, bool),
    ToggleWriteFor(Url, bool),
    AddSubscriptionToRelay(Url, Subscription),
    GetRelayInformation(Url),
    GetAllRelaysInformation,
    EoseActionsTo(Url, String, Vec<Subscription>),
    GetRelayStatusList(oneshot::Sender<RelayStatusList>),
    Auth(Url, Event),
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
        if let Err(e) = relay.run().await {
            log::debug!("{}", e);
        }
    });
    listeners.insert(url.clone(), RelayListener::new(relay_input_sender));
}

const EVENT_SENDER_CHANNEL_SIZE: usize = 10000;
const RELAY_TO_POOL_CHANNEL_SIZE: usize = 100;
const RELAY_INPUT_CHANNEL_SIZE: usize = 100;
