//! Local Discord RPC using an explicitly authorized, user-owned OAuth application.
//! No Discord session credentials or notification contents are exposed as data points.

use std::{
    env, fs,
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixStream, unix::OwnedWriteHalf},
    sync::{mpsc, watch},
    time::{self, Instant},
};

use crate::{
    config::Config,
    metrics::{DataPoint, DataValue},
};

const SOURCE: &str = "discord_source";
const MAX_FRAME: usize = 1024 * 1024;
const MAX_TOKEN_FILE: u64 = 16 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const RETRY_DELAY: Duration = Duration::from_secs(5);
const HEARTBEAT: Duration = Duration::from_secs(20);
static NEXT_NONCE: AtomicU64 = AtomicU64::new(1);

#[derive(Serialize, Deserialize)]
struct OAuthToken {
    client_id: String,
    access_token: String,
    refresh_token: String,
    token_type: String,
    scope: String,
    /// Seconds since the Unix epoch. Refresh requires the client's secret, which is never persisted.
    expires_at: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    token_type: String,
    scope: String,
    expires_in: u64,
}

impl OAuthToken {
    fn validate(&self, client_id: &str) -> Result<()> {
        ensure!(
            self.client_id == client_id,
            "Discord OAuth token belongs to a different client_id; authorize this application explicitly"
        );
        ensure!(
            !self.access_token.is_empty() && self.token_type.eq_ignore_ascii_case("Bearer"),
            "Discord OAuth token is invalid; authorize again"
        );
        let scopes: Vec<_> = self.scope.split_whitespace().collect();
        ensure!(
            ["rpc", "rpc.voice.read", "rpc.notifications.read"]
                .iter()
                .all(|scope| scopes.contains(scope)),
            "Discord OAuth token lacks rpc, rpc.voice.read or rpc.notifications.read; use an approved application/tester and authorize again"
        );
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        ensure!(
            self.expires_at > now.saturating_add(30),
            "Discord OAuth access token expired; run the explicit authorization command again (the client secret is not stored)"
        );
        Ok(())
    }
}

fn read_token(path: &Path, client_id: &str) -> Result<OAuthToken> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| {
            format!(
                "Discord OAuth token file {} unavailable; authorize explicitly",
                path.display()
            )
        })?;
    let metadata = file
        .metadata()
        .context("inspecting Discord OAuth token file")?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_TOKEN_FILE,
        "Discord OAuth token file must be a regular file of at most 16 KiB"
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
        "Discord OAuth token file must belong to this user and have mode 0600 or stricter"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_TOKEN_FILE + 1)
        .read_to_end(&mut bytes)
        .context("reading Discord OAuth token file")?;
    ensure!(
        bytes.len() as u64 <= MAX_TOKEN_FILE,
        "Discord OAuth token file is too large"
    );
    let token: OAuthToken = serde_json::from_slice(&bytes)
        .context("invalid Discord OAuth token JSON; authorize again")?;
    token.validate(client_id)?;
    Ok(token)
}

fn configured_token(config: &crate::config::Discord) -> Result<OAuthToken> {
    let client_id = config.client_id.trim();
    ensure!(
        !client_id.is_empty(),
        "Discord source needs discord.client_id for your approved RPC application/tester account"
    );
    let path = config.token_file.as_deref()
        .context("Discord source needs discord.token_file; explicitly authorize your approved RPC application first")?;
    read_token(path, client_id)
}

/// Check that the configured OAuth credentials are private, valid, scoped, and unexpired.
/// This does not connect to Discord or initiate authorization.
pub fn validate_credentials(config: &crate::config::Discord) -> Result<()> {
    configured_token(config).map(|_| ())
}

fn socket_paths() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    for key in ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"] {
        if let Some(path) = env::var_os(key)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            && !bases.contains(&path)
        {
            bases.push(path);
        }
    }
    let tmp = PathBuf::from("/tmp");
    if !bases.contains(&tmp) {
        bases.push(tmp);
    }
    let mut paths = Vec::new();
    for base in bases {
        for n in 0..10 {
            paths.push(base.join(format!("discord-ipc-{n}")));
            // Flatpak Discord exports its socket beneath the per-application runtime directory.
            paths.push(base.join(format!("app/com.discordapp.Discord/discord-ipc-{n}")));
        }
    }
    paths
}

fn check_peer(stream: &UnixStream) -> Result<()> {
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SO_PEERCRED is kernel-provided and cannot be forged by the socket endpoint.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error()).context("checking Discord IPC peer");
    }
    ensure!(
        size as usize == std::mem::size_of::<libc::ucred>(),
        "invalid Discord IPC peer credentials"
    );
    let credentials = unsafe { credentials.assume_init() };
    ensure!(
        credentials.uid == unsafe { libc::geteuid() },
        "Discord IPC socket belongs to another user"
    );
    Ok(())
}

async fn connect() -> Result<UnixStream> {
    let mut last_error = None;
    let search = async {
        for path in socket_paths() {
            match time::timeout(Duration::from_millis(350), UnixStream::connect(&path)).await {
                Ok(Ok(stream)) => {
                    if let Err(error) = check_peer(&stream) {
                        last_error = Some(error);
                        continue;
                    }
                    return Some(stream);
                }
                Ok(Err(error))
                    if error.kind() == io::ErrorKind::NotFound
                        || error.kind() == io::ErrorKind::ConnectionRefused => {}
                Ok(Err(_)) | Err(_) => {}
            }
        }
        None
    };
    if let Some(stream) = time::timeout(CONNECT_TIMEOUT, search).await.ok().flatten() {
        return Ok(stream);
    }
    if let Some(error) = last_error {
        return Err(error);
    }
    bail!(
        "Discord IPC socket unavailable; start Discord as this user and ensure your application is an approved RPC app or an invited tester"
    )
}

struct Packet {
    opcode: u32,
    payload: Vec<u8>,
}

async fn read_packet(reader: &mut (impl AsyncRead + Unpin)) -> Result<Packet> {
    let mut header = [0u8; 8];
    time::timeout(Duration::from_secs(45), reader.read_exact(&mut header))
        .await
        .context("Discord IPC header timed out")?
        .context("reading Discord IPC header")?;
    let opcode = u32::from_le_bytes(header[..4].try_into()?);
    let size = u32::from_le_bytes(header[4..].try_into()?) as usize;
    ensure!(size <= MAX_FRAME, "Discord IPC frame exceeds 1 MiB");
    let mut payload = vec![0; size];
    time::timeout(Duration::from_secs(15), reader.read_exact(&mut payload))
        .await
        .context("Discord IPC payload timed out")?
        .context("reading Discord IPC payload")?;
    Ok(Packet { opcode, payload })
}

async fn write_packet(
    writer: &mut (impl AsyncWrite + Unpin),
    opcode: u32,
    payload: &[u8],
) -> Result<()> {
    ensure!(
        payload.len() <= MAX_FRAME,
        "Discord IPC outbound frame exceeds 1 MiB"
    );
    let mut header = [0u8; 8];
    header[..4].copy_from_slice(&opcode.to_le_bytes());
    header[4..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    time::timeout(COMMAND_TIMEOUT, async {
        writer.write_all(&header).await?;
        writer.write_all(payload).await?;
        writer.flush().await
    })
    .await
    .context("Discord IPC write timed out")?
    .context("writing Discord IPC frame")?;
    Ok(())
}

fn parse_frame(packet: Packet) -> Result<Value> {
    ensure!(packet.opcode == 1, "unexpected Discord IPC opcode");
    let frame: Value =
        serde_json::from_slice(&packet.payload).context("invalid Discord IPC JSON")?;
    ensure!(frame.is_object(), "invalid Discord IPC payload");
    Ok(frame)
}

async fn handshake(mut stream: UnixStream, client_id: &str) -> Result<UnixStream> {
    let payload = serde_json::to_vec(&json!({ "v": 1, "client_id": client_id }))?;
    write_packet(&mut stream, 0, &payload).await?;
    let response = time::timeout(COMMAND_TIMEOUT, async {
        loop {
            let packet = read_packet(&mut stream).await?;
            match packet.opcode {
                1 => {
                    let frame = parse_frame(packet)?;
                    if frame["cmd"] == "DISPATCH" && frame["evt"] == "READY" { return Ok(()); }
                    if frame["evt"] == "ERROR" { bail!("Discord RPC handshake rejected; use an approved application or join its tester list"); }
                    bail!("unexpected Discord RPC handshake response");
                }
                3 => write_packet(&mut stream, 4, &packet.payload).await?,
                2 => bail!("Discord closed RPC handshake; check application approval and tester access"),
                _ => bail!("unexpected Discord IPC handshake opcode"),
            }
        }
    }).await.context("Discord RPC handshake timed out")?;
    response?;
    Ok(stream)
}

// Abort the reader if a config change or shutdown drops the session future.
struct ReaderTask(tokio::task::JoinHandle<()>);
impl Drop for ReaderTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Rpc {
    writer: OwnedWriteHalf,
    incoming: mpsc::Receiver<Result<Packet>>,
    _reader: ReaderTask,
}

enum Incoming {
    Frame(Value),
    Pong,
}

impl Rpc {
    fn new(stream: UnixStream) -> Self {
        let (mut reader, writer) = stream.into_split();
        let (sender, incoming) = mpsc::channel(8);
        let task = tokio::spawn(async move {
            loop {
                let packet = read_packet(&mut reader).await;
                let failed = packet.is_err();
                if sender.send(packet).await.is_err() || failed {
                    break;
                }
            }
        });
        Self {
            writer,
            incoming,
            _reader: ReaderTask(task),
        }
    }

    async fn receive(&mut self) -> Result<Incoming> {
        loop {
            let packet = self
                .incoming
                .recv()
                .await
                .context("Discord IPC connection closed")??;
            match packet.opcode {
                1 => return Ok(Incoming::Frame(parse_frame(packet)?)),
                2 => bail!("Discord closed its RPC connection"),
                3 => write_packet(&mut self.writer, 4, &packet.payload).await?,
                4 => return Ok(Incoming::Pong),
                _ => bail!("unexpected Discord IPC opcode"),
            }
        }
    }

    async fn command(
        &mut self,
        cmd: &str,
        args: Value,
        event: Option<&str>,
        state: &mut State,
        tx: &watch::Sender<Vec<DataPoint>>,
        deadline: Duration,
    ) -> Result<Value> {
        let nonce = format!(
            "open-wbg-service-{}",
            NEXT_NONCE.fetch_add(1, Ordering::Relaxed)
        );
        let mut request = json!({ "cmd": cmd, "args": args, "nonce": nonce });
        if let Some(event) = event {
            request["evt"] = event.into();
        }
        let body = serde_json::to_vec(&request)?;
        time::timeout(deadline, async {
            write_packet(&mut self.writer, 1, &body).await?;
            loop {
                match self.receive().await? {
                    Incoming::Pong => continue,
                    Incoming::Frame(frame) if frame["nonce"].as_str() == Some(nonce.as_str()) => {
                        if frame["evt"] == "ERROR" {
                            let code = frame["data"]["code"].as_i64().map_or_else(|| "unknown".to_owned(), |n| n.to_string());
                            bail!("Discord RPC {cmd} rejected (code {code}); check the app's RPC approval/tester access and granted scopes");
                        }
                        ensure!(frame["cmd"] == cmd, "Discord RPC reply command does not match its nonce");
                        if let Some(event) = event {
                            ensure!(frame["data"]["evt"] == event, "Discord RPC subscription acknowledgment does not match the requested event");
                        }
                        return Ok(frame);
                    }
                    Incoming::Frame(frame) if frame["cmd"] == "DISPATCH" => state.dispatch(&frame, tx),
                    Incoming::Frame(frame) if frame["evt"] == "ERROR" => {
                        let code = frame["data"]["code"].as_i64().map_or_else(|| "unknown".to_owned(), |n| n.to_string());
                        bail!("Discord RPC error (code {code}); check the app's RPC approval/tester access and granted scopes");
                    }
                    Incoming::Frame(_) => bail!("unexpected Discord RPC reply nonce"),
                }
            }
        }).await.with_context(|| format!("Discord RPC {cmd} timed out"))?
    }
}

#[derive(Default)]
struct State {
    muted: Option<bool>,
    notification: Option<i64>,
    next_notification: i64,
    voice_subscribed: bool,
    notification_subscribed: bool,
}

impl State {
    fn dispatch(&mut self, frame: &Value, tx: &watch::Sender<Vec<DataPoint>>) {
        if frame["cmd"] != "DISPATCH" {
            return;
        }
        match frame["evt"].as_str() {
            Some("VOICE_SETTINGS_UPDATE") if self.voice_subscribed => {
                self.muted = frame["data"]["mute"].as_bool();
                self.publish(tx);
            }
            Some("NOTIFICATION_CREATE") if self.notification_subscribed => {
                self.next_notification = if self.next_notification == i64::MAX {
                    1
                } else {
                    self.next_notification + 1
                };
                self.notification = Some(self.next_notification);
                self.publish(tx);
            }
            _ => {}
        }
    }

    fn publish(&self, tx: &watch::Sender<Vec<DataPoint>>) {
        let mut points = Vec::with_capacity(2);
        if let Some(muted) = self.muted {
            points.push(DataPoint {
                path: "discord/mic/muted".into(),
                name: "Microphone muted".into(),
                source: SOURCE.into(),
                value: DataValue::Bool(muted),
                min: 0.0,
                max: 1.0,
            });
        }
        if let Some(count) = self.notification {
            points.push(DataPoint {
                path: "discord/notification".into(),
                name: "Discord notification".into(),
                source: SOURCE.into(),
                value: DataValue::Integer(count),
                min: 0.0,
                max: i64::MAX as f32,
            });
        }
        tx.send_replace(points);
    }
}

async fn session(
    client_id: &str,
    token: &OAuthToken,
    tx: &watch::Sender<Vec<DataPoint>>,
    next_notification: &mut i64,
) -> Result<()> {
    let expiry = Instant::now()
        + Duration::from_secs(
            token
                .expires_at
                .saturating_sub(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()),
        );
    let stream = handshake(connect().await?, client_id).await?;
    let mut rpc = Rpc::new(stream);
    let mut state = State {
        next_notification: *next_notification,
        ..State::default()
    };
    rpc.command(
        "AUTHENTICATE",
        json!({"access_token": token.access_token}),
        None,
        &mut state,
        tx,
        COMMAND_TIMEOUT,
    )
    .await?;
    state.voice_subscribed = true;
    rpc.command(
        "SUBSCRIBE",
        json!({}),
        Some("VOICE_SETTINGS_UPDATE"),
        &mut state,
        tx,
        COMMAND_TIMEOUT,
    )
    .await?;
    state.notification_subscribed = true;
    rpc.command(
        "SUBSCRIBE",
        json!({}),
        Some("NOTIFICATION_CREATE"),
        &mut state,
        tx,
        COMMAND_TIMEOUT,
    )
    .await?;
    let voice = rpc
        .command(
            "GET_VOICE_SETTINGS",
            json!({}),
            None,
            &mut state,
            tx,
            COMMAND_TIMEOUT,
        )
        .await?;
    state.muted = voice["data"]["mute"].as_bool();
    state.publish(tx);
    *next_notification = state.next_notification;
    let mut heartbeat = time::interval_at(Instant::now() + HEARTBEAT, HEARTBEAT);
    heartbeat.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut waiting_for_pong = false;
    loop {
        tokio::select! {
            _ = time::sleep_until(expiry) => bail!("Discord OAuth access token expired; authorize again"),
            _ = heartbeat.tick() => {
                ensure!(!waiting_for_pong, "Discord IPC heartbeat timed out");
                write_packet(&mut rpc.writer, 3, &[]).await?;
                waiting_for_pong = true;
            }
            result = rpc.receive() => {
                match result? {
                    Incoming::Pong => waiting_for_pong = false,
                    Incoming::Frame(frame) if frame["cmd"] == "DISPATCH" => {
                        state.dispatch(&frame, tx);
                        *next_notification = state.next_notification;
                    }
                    Incoming::Frame(frame) if frame["evt"] == "ERROR" => {
                        let code = frame["data"]["code"].as_i64().map_or_else(|| "unknown".to_owned(), |n| n.to_string());
                        bail!("Discord RPC connection error (code {code})");
                    }
                    Incoming::Frame(_) => bail!("unexpected Discord RPC frame without an outstanding command"),
                }
            }
        }
    }
}

/// Run only while the configured Discord source is enabled. This task does not block HID or API work.
pub async fn run(
    mut config: watch::Receiver<Arc<Config>>,
    tx: watch::Sender<Vec<DataPoint>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut last_error = None;
    let mut next_notification = 0;
    loop {
        if *shutdown.borrow() {
            tx.send_replace(Vec::new());
            return Ok(());
        }
        let selected = config.borrow().clone();
        if !selected.enabled_sources.contains(SOURCE) {
            last_error = None;
            tx.send_replace(Vec::new());
            tokio::select! {
                result = config.changed() => { if result.is_err() { return Ok(()); } }
                result = shutdown.changed() => { if result.is_err() { return Ok(()); } }
            }
            continue;
        }
        let client_id = selected.discord.client_id.trim();
        let result = async {
            let token = configured_token(&selected.discord)?;
            session(client_id, &token, &tx, &mut next_notification).await
        };
        tokio::select! {
            outcome = result => {
                tx.send_replace(Vec::new());
                if let Err(error) = outcome {
                    let description = format!("{error:#}");
                    if last_error.as_ref() != Some(&description) {
                        tracing::warn!("Discord source unavailable: {description}");
                        last_error = Some(description);
                    }
                }
            }
            result = config.changed() => { if result.is_err() { tx.send_replace(Vec::new()); return Ok(()); } continue; }
            result = shutdown.changed() => { if result.is_err() { tx.send_replace(Vec::new()); return Ok(()); } continue; }
        }
        tokio::select! {
            _ = time::sleep(RETRY_DELAY) => {}
            result = config.changed() => { if result.is_err() { tx.send_replace(Vec::new()); return Ok(()); } }
            result = shutdown.changed() => { if result.is_err() { tx.send_replace(Vec::new()); return Ok(()); } }
        }
    }
}

/// Ask the local Discord client for consent and exchange its code for OAuth credentials.
/// Invoke only from an explicit CLI authorization action; the daemon never opens a consent dialog.
pub async fn authorize(
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
    token_path: &Path,
) -> Result<()> {
    ensure!(
        !client_id.trim().is_empty() && !client_secret.is_empty() && !redirect_uri.is_empty(),
        "Discord authorization needs your application's client ID, secret and registered redirect URI"
    );
    let stream = handshake(connect().await?, client_id).await?;
    let mut rpc = Rpc::new(stream);
    let mut state = State::default();
    let (unused_tx, _) = watch::channel(Vec::new());
    // AUTHORIZE is the only RPC operation allowed before AUTHENTICATE; it prompts the user in Discord.
    let reply = rpc
        .command(
            "AUTHORIZE",
            json!({
                "client_id": client_id,
                "scopes": ["rpc", "rpc.voice.read", "rpc.notifications.read"]
            }),
            None,
            &mut state,
            &unused_tx,
            Duration::from_secs(300),
        )
        .await?;
    let code = reply["data"]["code"]
        .as_str()
        .context("Discord authorization returned no code; check RPC approval/tester access")?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("creating Discord OAuth HTTPS client")?;
    let mut response = http.post("https://discord.com/api/oauth2/token")
        .basic_auth(client_id, Some(client_secret))
        .form(&[("grant_type", "authorization_code"), ("code", code), ("redirect_uri", redirect_uri)])
        .send().await.map_err(|_| anyhow::anyhow!("Discord OAuth token exchange failed (network/TLS timeout); verify network and registered redirect URI"))?;
    ensure!(
        response.status().is_success(),
        "Discord OAuth token exchange rejected (HTTP {}); verify the client secret, registered redirect URI, and RPC approval/tester access",
        response.status()
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("reading Discord OAuth response failed"))?
    {
        ensure!(
            chunk.len() <= (MAX_TOKEN_FILE as usize).saturating_sub(bytes.len()),
            "Discord OAuth token response exceeds 16 KiB"
        );
        bytes.extend_from_slice(&chunk);
    }
    let response: TokenResponse =
        serde_json::from_slice(&bytes).context("invalid Discord OAuth token response")?;
    ensure!(
        response.expires_in > 0,
        "Discord OAuth token has no valid lifetime"
    );
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let token = OAuthToken {
        client_id: client_id.to_owned(),
        access_token: response.access_token,
        refresh_token: response.refresh_token,
        token_type: response.token_type,
        scope: response.scope,
        expires_at: now
            .checked_add(response.expires_in)
            .context("Discord OAuth expiry overflow")?,
    };
    token.validate(client_id)?;
    save_token(token_path, &token)
}

fn save_token(path: &Path, token: &OAuthToken) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .with_context(|| format!("creating Discord token directory {}", parent.display()))?;
    let name = path
        .file_name()
        .context("Discord token path must name a file")?
        .to_string_lossy();
    let bytes = serde_json::to_vec(token)?;
    for _ in 0..4 {
        let nonce = NEXT_NONCE.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), nonce));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp);
        let mut file = match file {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("creating private Discord OAuth token file"),
        };
        let result = (|| -> Result<()> {
            file.write_all(&bytes)
                .context("writing Discord OAuth token file")?;
            file.sync_all()
                .context("syncing Discord OAuth token file")?;
            fs::rename(&temp, path).context("atomically installing Discord OAuth token file")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result?;
        return Ok(());
    }
    bail!("unable to allocate private Discord OAuth token file")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ipc_header_uses_little_endian_opcode_and_length() {
        let (mut sender, mut receiver) = tokio::io::duplex(64);
        write_packet(&mut sender, 3, b"ping").await.unwrap();
        let mut header = [0; 8];
        receiver.read_exact(&mut header).await.unwrap();
        assert_eq!(header, [3, 0, 0, 0, 4, 0, 0, 0]);
        let mut payload = [0; 4];
        receiver.read_exact(&mut payload).await.unwrap();
        assert_eq!(&payload, b"ping");

        sender.write_all(&[1, 0, 0, 0, 1, 0, 16, 0]).await.unwrap();
        assert!(read_packet(&mut receiver).await.is_err());
    }

    #[test]
    fn events_publish_mute_and_counter_without_notification_contents() {
        let (tx, rx) = watch::channel(Vec::new());
        let mut state = State {
            voice_subscribed: true,
            notification_subscribed: true,
            ..State::default()
        };
        state.dispatch(
            &json!({"cmd":"DISPATCH", "evt":"VOICE_SETTINGS_UPDATE", "data":{"mute":true}}),
            &tx,
        );
        assert!(matches!(&rx.borrow()[0].value, DataValue::Bool(true)));
        state.dispatch(
            &json!({"cmd":"DISPATCH", "evt":"NOTIFICATION_CREATE",
            "data":{"message":{"content":"private marker"}}}),
            &tx,
        );
        assert!(matches!(&rx.borrow()[1].value, DataValue::Integer(1)));
        assert!(
            !serde_json::to_string(&*rx.borrow())
                .unwrap()
                .contains("private marker")
        );
        state.dispatch(
            &json!({"cmd":"DISPATCH", "evt":"NOTIFICATION_CREATE", "data":{}}),
            &tx,
        );
        assert!(matches!(&rx.borrow()[1].value, DataValue::Integer(2)));
        state.dispatch(
            &json!({"cmd":"DISPATCH", "evt":"VOICE_SETTINGS_UPDATE", "data":{}}),
            &tx,
        );
        assert_eq!(rx.borrow().len(), 1); // Unknown mute is removed, not reported as false.
    }
}
