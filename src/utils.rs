use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use url::Url;

pub type ConnResult =
    Result<WebSocketStream<MaybeTlsStream<TcpStream>>, Box<dyn std::error::Error + Send>>;

pub fn spawn_get_connection(url: Url, one_tx: mpsc::Sender<ConnResult>) {
    tokio::spawn(async move {
        // Connect to the server
        log::trace!("GET CONN");

        match connect_async(&url).await {
            Ok((ws_stream, _)) => {
                // Split the stream into a sink and a stream
                if let Err(e) = one_tx.send(Ok(ws_stream)).await {
                    log::info!("Error sending ok to one_rx: {}", e);
                }
            }
            Err(e) => {
                if let Err(e) = one_tx.send(Err(Box::new(e))).await {
                    log::info!("Error sending err to one_rx: {}", e);
                }
            }
        }
    });

    // tokio::spawn(async move {
    //     connect_async(&url)
    //         .await
    //         .map(|r| r.0)
    //         .map_err(|e| e.to_string())
    // })
}
