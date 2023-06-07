use nostr::prelude::RelayInformationDocument;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use url::Url;

pub type ConnResult =
    Result<WebSocketStream<MaybeTlsStream<TcpStream>>, Box<dyn std::error::Error + Send>>;
pub type DocResult = Result<RelayInformationDocument, Box<dyn std::error::Error + Send>>;

pub fn spawn_get_connection(url: Url, conn_tx: oneshot::Sender<ConnResult>) {
    tokio::spawn(async move {
        // Connect to the server
        log::trace!("GET CONN");
        match connect_async(&url).await {
            Ok((ws_stream, _)) => {
                if let Err(_) = conn_tx.send(Ok(ws_stream)) {
                    log::info!("Error sending conn ok to relay: {}", url);
                }
            }
            Err(e) => {
                if let Err(_) = conn_tx.send(Err(Box::new(e))) {
                    log::info!("Error sending conn error to relay: {}", url);
                }
            }
        }
    });
}

pub fn spawn_get_document(url: Url, doc_tx: oneshot::Sender<DocResult>) {
    tokio::spawn(async move {
        let document = RelayInformationDocument::get(url.clone(), None).await;
        match document {
            Ok(document) => {
                if let Err(_) = doc_tx.send(Ok(document)) {
                    log::info!("Error sending doc ok to relay: {}", url);
                }
            }
            Err(e) => {
                if let Err(_) = doc_tx.send(Err(Box::new(e))) {
                    log::info!("Error sending doc error to relay: {}", url);
                }
            }
        };
    });
}
