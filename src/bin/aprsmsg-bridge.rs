//! WebSocket ↔ TCP bridge so a browser can talk to Direwolf's KISS port.
//!
//!   aprsmsg-bridge [--listen 127.0.0.1:8765] [--tcp 127.0.0.1:8001]

use std::env;
use std::net::SocketAddr;
use std::process;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

const DEFAULT_LISTEN: &str = "127.0.0.1:8765";
const DEFAULT_TCP: &str = "127.0.0.1:8001";

#[tokio::main]
async fn main() {
    let (listen, tcp) = parse_args();
    let listen_addr: SocketAddr = listen.parse().unwrap_or_else(|e| {
        eprintln!("aprsmsg-bridge: bad --listen {listen:?}: {e}");
        process::exit(2);
    });
    let tcp_target = Arc::new(tcp);

    let listener = TcpListener::bind(listen_addr).await.unwrap_or_else(|e| {
        eprintln!("aprsmsg-bridge: cannot bind {listen_addr}: {e}");
        process::exit(1);
    });
    eprintln!(
        "aprsmsg-bridge: ws://{listen_addr} → tcp://{}",
        tcp_target.as_str()
    );
    eprintln!(
        "aprsmsg-bridge: start Direwolf first (KISS TCP on {}) before connecting from the browser",
        tcp_target.as_str()
    );

    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            continue;
        };
        let target = Arc::clone(&tcp_target);
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, target.as_str()).await {
                eprintln!("aprsmsg-bridge: {peer}: {e}");
            }
        });
    }
}

async fn handle_client(
    stream: TcpStream,
    tcp_addr: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut ws = accept_async(stream).await?;
    let tcp = match TcpStream::connect(tcp_addr).await {
        Ok(tcp) => tcp,
        Err(e) => {
            let reason = format!(
                "Direwolf not reachable at {tcp_addr} ({e}) — start Direwolf with KISS TCP enabled"
            );
            let _ = ws
                .close(Some(CloseFrame {
                    code: CloseCode::Error,
                    reason: reason.clone().into(),
                }))
                .await;
            return Err(reason.into());
        }
    };
    tcp.set_nodelay(true)?;
    let (mut tcp_rd, mut tcp_wr) = tcp.into_split();
    let (ws_tx, mut ws_rx) = ws.split();
    let ws_tx = Arc::new(Mutex::new(ws_tx));

    let to_tcp = {
        let ws_tx = Arc::clone(&ws_tx);
        async move {
            while let Some(msg) = ws_rx.next().await {
                let msg = msg?;
                match msg {
                    Message::Binary(data) => tcp_wr.write_all(&data).await?,
                    Message::Text(text) => tcp_wr.write_all(text.as_bytes()).await?,
                    Message::Ping(p) => {
                        ws_tx.lock().await.send(Message::Pong(p)).await?;
                    }
                    Message::Close(_) => break,
                    Message::Pong(_) | Message::Frame(_) => {}
                }
            }
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        }
    };

    let to_ws = async move {
        let mut buf = [0u8; 4096];
        loop {
            let n = tcp_rd.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            ws_tx
                .lock()
                .await
                .send(Message::Binary(buf[..n].to_vec().into()))
                .await?;
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    };

    tokio::select! {
        r = to_tcp => r?,
        r = to_ws => r?,
    }
    Ok(())
}

fn parse_args() -> (String, String) {
    let mut listen = DEFAULT_LISTEN.to_owned();
    let mut tcp = DEFAULT_TCP.to_owned();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => {
                listen = args.next().unwrap_or_else(|| die("--listen needs a value"))
            }
            "--tcp" => tcp = args.next().unwrap_or_else(|| die("--tcp needs a value")),
            "-h" | "--help" => {
                println!(
                    "usage: aprsmsg-bridge [--listen {DEFAULT_LISTEN}] [--tcp {DEFAULT_TCP}]\n\n\
                     Proxies browser WebSocket clients to Direwolf's KISS TCP port."
                );
                process::exit(0);
            }
            other => die(&format!("unknown argument {other:?}")),
        }
    }
    (listen, tcp)
}

fn die(msg: &str) -> ! {
    eprintln!("aprsmsg-bridge: {msg}");
    process::exit(2);
}
