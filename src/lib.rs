use futures_util::stream::SplitSink;
use futures_util::stream::SplitStream;
use futures_util::SinkExt;
use futures_util::StreamExt;
use nostr::ClientMessage;
use nostr::Filter;
use nostr::RelayMessage;
use nostr::SubscriptionId;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use url::Url;

mod error;
mod state_machine;
mod stats;
pub use error::Error;
pub use state_machine::{run_pool, PoolEvent, PoolInput, PoolState, RelayEvent};
pub use stats::RelayConnectionStats;

#[derive(Debug, Clone)]
pub enum ToMain {
    Connected { url: Url },
    Disconnected { url: Url },
    RelayMessage { url: Url, message: RelayMessage },
    Error { url: Url, message: String },
    Close,
}

#[derive(Debug, Clone)]
pub struct Relay {
    pub url: Url,
    pub inner_rx: Arc<Mutex<mpsc::Receiver<String>>>,
    pub inner_tx: Arc<Mutex<mpsc::Sender<String>>>,
    pub to_main_tx: Arc<Mutex<mpsc::Sender<ToMain>>>,
    pub one_rx: Arc<Mutex<mpsc::Receiver<ConnResult>>>,
    pub ws_read: Arc<Mutex<Option<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>>>>,
    pub ws_write:
        Arc<Mutex<Option<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>>,
    pub attempts: u8,
    pub stats: RelayConnectionStats,
    pub msg_queue: Arc<Mutex<VecDeque<ClientMessage>>>,
}
impl Relay {
    pub fn new(url: &Url, main_tx: &mut mpsc::Sender<ToMain>) -> Self {
        let (inner_tx, inner_rx) = mpsc::channel(10);
        let (one_tx, one_rx) = mpsc::channel(1);

        get_conn(url, one_tx);

        Self {
            url: url.to_owned(),
            inner_rx: Arc::new(Mutex::new(inner_rx)),
            inner_tx: Arc::new(Mutex::new(inner_tx)),
            to_main_tx: Arc::new(Mutex::new(main_tx.clone())),
            ws_write: Arc::new(Mutex::new(None)),
            ws_read: Arc::new(Mutex::new(None)),
            one_rx: Arc::new(Mutex::new(one_rx)),
            attempts: 0,
            stats: RelayConnectionStats::default(),
            msg_queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    async fn set_ws_pair(&self, ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>) {
        log::info!("Setting ws pair");
        let (ws_write, ws_read) = ws_stream.split();
        let mut wrt = self.ws_write.lock().await;
        *wrt = Some(ws_write);

        let mut rd = self.ws_read.lock().await;
        *rd = Some(ws_read);
    }

    pub fn run(&self) {
        log::info!("Running relay: {}", self.url);

        let relay = self.clone();
        tokio::spawn(receive_connection(relay));

        let relay = self.clone();
        tokio::spawn(process_streams(relay));
    }

    async fn send_message(&self, message: ClientMessage) -> Result<(), Error> {
        // Lock the mutex around ws_write
        let mut ws_write = self.ws_write.lock().await;

        // Check if there is a WebSocket connection
        let ws_write = match ws_write.as_mut() {
            Some(ws_write) => ws_write,
            None => {
                self.msg_queue.lock().await.push_back(message);
                return Err(Error::NoWebsocketConnection(self.url.to_string()));
            }
        };

        // Send the message
        ws_write
            .send(Message::text(message.as_json()))
            .await
            .map_err(|e| e.into())
    }
    async fn subscribe(&self, filters: Vec<Filter>) {
        let sub_id = SubscriptionId::new("abc");
        let message = ClientMessage::new_req(sub_id, filters);
        if let Err(e) = self.send_message(message).await {
            log::error!("Error sending message: {} - {}", self.url, e);
        }
    }
    async fn unsubscribe(&self) {
        let sub_id = SubscriptionId::new("abc");
        let message = ClientMessage::close(sub_id);
        if let Err(e) = self.send_message(message).await {
            log::error!("Error sending message: {} - {}", self.url, e);
        }
    }
    /// Disconnect from relay and set status to 'Disconnected'
    async fn close(&self) -> Result<(), Error> {
        let inner_tx = self.inner_tx.lock().await;
        inner_tx
            .send("close".to_owned())
            .await
            .map_err(|e| Error::SendError(e.to_string()))
    }
}

struct RelayPoolRunner {
    main_rx: mpsc::Receiver<ToMain>,
    notification_sender: broadcast::Sender<RelayPoolNotification>,
}
impl RelayPoolRunner {
    pub async fn run(&mut self) {
        // Keep the main function running until it receives a close or error message
        while let Some(message) = self.main_rx.recv().await {
            match message {
                ToMain::RelayMessage { url, message } => {
                    self.notification_sender
                        .send(RelayPoolNotification::RelayMessage { url, message })
                        .unwrap();
                }
                ToMain::Connected { url } => {
                    self.notification_sender
                        .send(RelayPoolNotification::RelayConnected(url))
                        .unwrap();
                }
                ToMain::Disconnected { url } => {
                    self.notification_sender
                        .send(RelayPoolNotification::RelayDisconnected(url))
                        .unwrap();
                }
                ToMain::Error { url, message } => {
                    self.notification_sender
                        .send(RelayPoolNotification::Error { url, message })
                        .unwrap();
                }
                ToMain::Close => {
                    self.notification_sender
                        .send(RelayPoolNotification::Shutdown)
                        .unwrap();
                    self.main_rx.close();
                    break;
                }
            }
        }
    }

    fn new(
        main_rx: mpsc::Receiver<ToMain>,
        notification_sender: broadcast::Sender<RelayPoolNotification>,
    ) -> Self {
        Self {
            main_rx,
            notification_sender,
        }
    }
}

#[derive(Debug, Clone)]
pub enum RelayPoolNotification {
    RelayMessage { url: Url, message: RelayMessage },
    Shutdown,
    RelayConnected(Url),
    RelayDisconnected(Url),
    Error { url: Url, message: String },
}

#[derive(Debug, Clone)]
pub struct RelayPool {
    relays: Vec<Arc<Mutex<Relay>>>,
    main_tx: mpsc::Sender<ToMain>,
    notification_sender: broadcast::Sender<RelayPoolNotification>,
}
impl RelayPool {
    pub fn new() -> Self {
        let (notification_sender, _) = broadcast::channel(1024);
        let (main_tx, main_rx) = mpsc::channel(1024);

        let mut relay_pool_runner = RelayPoolRunner::new(main_rx, notification_sender.clone());
        tokio::spawn(async move {
            relay_pool_runner.run().await;
        });

        Self {
            relays: Vec::new(),
            main_tx,
            notification_sender,
        }
    }
    /// Get new notification listener
    pub fn notifications(&self) -> broadcast::Receiver<RelayPoolNotification> {
        self.notification_sender.subscribe()
    }
    pub async fn subscribe(&self, filters: Vec<Filter>) {
        for r in &self.relays {
            let relay = r.lock().await;
            relay.subscribe(filters.clone()).await;
        }
    }
    pub async fn close(&self) -> Result<(), Error> {
        for r in &self.relays {
            let relay = r.lock().await;
            if let Err(e) = relay.close().await {
                log::error!("Error closing relay: {}", e);
            }
        }
        let msg = ToMain::Close;
        Ok(self
            .main_tx
            .send(msg.to_owned())
            .await
            .map_err(|_e| Error::FailedToSendToMain(msg.clone()))?)
    }
    pub fn add_relay(&mut self, url: &str) -> Result<(), Error> {
        log::info!("Adding relay {}", url);
        let url = Url::parse(url)?;
        let relay = Relay::new(&url, &mut self.main_tx);
        relay.run();
        self.relays.push(Arc::new(Mutex::new(relay)));
        Ok(())
    }

    pub async fn send_message(&self, message: ClientMessage) -> Result<(), Error> {
        for relay in &self.relays {
            let relay = relay.lock().await;
            relay.send_message(message.clone()).await?;
        }
        Ok(())
    }
}

pub enum ConnResult {
    Ok(WebSocketStream<MaybeTlsStream<TcpStream>>),
    Error(String),
}

fn get_conn(url: &Url, one_tx: mpsc::Sender<ConnResult>) {
    let url_1 = url.clone();
    tokio::spawn(async move {
        // Connect to the server
        log::debug!("GET CONN");
        match connect_async(&url_1).await {
            Ok((ws_stream, _)) => {
                // Split the stream into a sink and a stream
                if let Err(e) = one_tx.send(ConnResult::Ok(ws_stream)).await {
                    log::error!("Error sending conn result: {}", e);
                }
            }
            Err(e) => {
                if let Err(e) = one_tx.send(ConnResult::Error(e.to_string())).await {
                    log::error!("Error sending conn error: {}", e);
                }
            }
        }
    });
}

async fn receive_connection(relay: Relay) -> Result<(), Error> {
    let mut one_rx = relay.one_rx.lock().await;
    while let Some(conn) = one_rx.recv().await {
        match conn {
            ConnResult::Ok(ws_stream) => {
                log::info!("CONN OK");
                relay.stats.new_success();
                relay.set_ws_pair(ws_stream).await;
                relay
                    .inner_tx
                    .lock()
                    .await
                    .send("conn".to_string())
                    .await
                    .unwrap();
                let main_tx = relay.to_main_tx.lock().await;
                main_tx
                    .send(ToMain::Connected {
                        url: relay.url.to_owned(),
                    })
                    .await
                    .unwrap();
                break;
            }
            ConnResult::Error(e) => {
                log::error!("Error connecting to {}: {}", relay.url, e);
                let main_tx = relay.to_main_tx.lock().await;
                main_tx
                    .send(ToMain::Error {
                        url: relay.url.to_owned(),
                        message: e.to_string(),
                    })
                    .await
                    .unwrap();
                break;
            }
        }
    }
    Ok(())
}

async fn process_streams(relay: Relay) -> Result<(), Error> {
    loop {
        {
            let mut ws_read = relay.ws_read.lock().await;
            if let Some(ws_read) = ws_read.as_mut() {
                let mut inner_rx = relay.inner_rx.lock().await;
                tokio::select! {
                    inner = inner_rx.recv() => {
                        if let Some(inner_msg) = inner {
                            match inner_msg.as_str() {
                                "conn" => {
                                    let mut msg_q = relay.msg_queue.lock().await;
                                    while let Some(msg) = msg_q.pop_front() {
                                        if let Err(e) = relay.send_message(msg).await {
                                            log::error!("Error sending message: {} - {}", &relay.url, e);
                                            break;
                                        }
                                    }
                                }
                                "close" => {
                                    relay.unsubscribe().await;
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                    received_ws = ws_read.next() => {
                        if let Some(received_ws_msg) = received_ws {
                            match received_ws_msg {
                                Ok(msg) if msg.is_text() => {
                                    // Handle incoming messages here
                                    match msg.to_text() {
                                        Ok(msg_text) => match RelayMessage::from_json(msg_text) {
                                            Ok(relay_message) => {
                                                relay
                                                    .to_main_tx
                                                    .lock()
                                                    .await
                                                    .send(ToMain::RelayMessage {
                                                        url: relay.url.to_owned(),
                                                        message: relay_message,
                                                    })
                                                    .await
                                                    .unwrap();
                                            }
                                            Err(e) => {
                                                relay
                                                    .to_main_tx
                                                    .lock()
                                                    .await
                                                    .send(ToMain::Error {
                                                        url: relay.url.to_owned(),
                                                        message: e.to_string(),
                                                    })
                                                    .await
                                                    .unwrap();
                                            }
                                        },
                                        Err(e) => {
                                            let main_tx = relay.to_main_tx.lock().await;
                                            main_tx
                                                .send(ToMain::Error {
                                                    url: relay.url.to_owned(),
                                                    message: e.to_string(),
                                                })
                                                .await
                                                .unwrap();
                                        }
                                    }
                                }
                                Ok(msg) if msg.is_close() => {
                                    let main_tx = relay.to_main_tx.lock().await;
                                    main_tx
                                        .send(ToMain::Disconnected {
                                            url: relay.url.to_owned(),
                                        })
                                        .await
                                        .unwrap();
                                }
                                Err(e) => {
                                    let main_tx = relay.to_main_tx.lock().await;
                                    main_tx
                                        .send(ToMain::Error {
                                            url: relay.url.to_owned(),
                                            message: e.to_string(),
                                        })
                                        .await
                                        .unwrap();
                                }
                                _ => (),
                            }
                        }

                    }
                }
            }
        }
    }
    Ok(())
}
