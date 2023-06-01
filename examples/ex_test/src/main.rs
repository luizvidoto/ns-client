use std::{collections::HashSet, path::Path, str::FromStr, time::Duration};

use log::debug;
use nostr::{
    nips::nip21::NostrURI,
    prelude::{FromBech32, FromPkStr, FromSkStr},
    ChannelId, EventId, Keys, RelayMessage, SubscriptionId, Tag, Url,
};
use tokio::{
    fs::OpenOptions,
    io::{AsyncWriteExt, BufWriter},
    sync::mpsc,
};

#[tokio::main]
async fn main() {
    env_logger::init();

    let (tx, rx) = mpsc::channel(100);
    let file_path = Path::new("output.json");

    let receiver_task = tokio::spawn(async move {
        receive_and_write(rx, file_path).await.unwrap();
    });

    tx.send("blablabla".to_string()).await.unwrap();

    tx.send("END".to_string()).await.unwrap();

    receiver_task.await.unwrap();

    log::info!("APP: Done");
}

async fn receive_and_write(
    mut receiver: mpsc::Receiver<String>,
    file_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(file_path)
        .await?;

    log::info!("APP: Writing to file: {:?}", file_path);

    let mut writer = BufWriter::new(file);

    let mut first = true;

    while let Some(data) = receiver.recv().await {
        if data == "END" {
            // Write the closing bracket and break the loop
            writer.write_all(b"]").await?;
            break;
        }

        let json = if first {
            first = false;
            format!("[\n{}\n", data)
        } else {
            format!(",{}\n", data)
        };

        writer.write_all(json.as_bytes()).await?;
    }

    writer.flush().await?;

    Ok(())
}
