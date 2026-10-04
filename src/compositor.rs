use std::{collections::HashMap, env, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::watch,
};
use tokio_util::codec::{FramedRead, LinesCodec};

const MAX_MESSAGE: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[default]
    Auto,
    Niri,
    Sway,
    Hyprland,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub app_id: String,
    pub title: String,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Focus {
    pub connected: bool,
    pub window: Option<Window>,
}

impl Backend {
    pub fn resolve(self) -> Result<Self> {
        match self {
            Self::Auto if env::var_os("NIRI_SOCKET").is_some() => Ok(Self::Niri),
            Self::Auto if env::var_os("SWAYSOCK").is_some() => Ok(Self::Sway),
            Self::Auto if env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() => {
                Ok(Self::Hyprland)
            }
            Self::Auto => bail!(
                "no supported compositor socket in environment; start inside Niri/Sway/Hyprland or import its environment into systemd --user"
            ),
            backend => Ok(backend),
        }
    }
}

/// On-demand inventory for Wootility's app picker; never used to poll focus.
pub async fn windows(backend: Backend) -> Result<Vec<Window>> {
    tokio::time::timeout(Duration::from_secs(5), async {
        match backend.resolve()? {
            Backend::Niri => {
                let path = env::var_os("NIRI_SOCKET").context("NIRI_SOCKET is not set")?;
                let mut socket = UnixStream::connect(path).await?;
                socket.write_all(b"\"Windows\"\n").await?;
                let mut lines =
                    FramedRead::new(socket, LinesCodec::new_with_max_length(MAX_MESSAGE));
                let reply: Value =
                    serde_json::from_str(&lines.next().await.context("Niri closed")??)?;
                let windows: Vec<NiriWindow> =
                    serde_json::from_value(reply["Ok"]["Windows"].clone())
                        .with_context(|| format!("Niri rejected window query: {reply}"))?;
                Ok(windows
                    .into_iter()
                    .map(|w| Window {
                        app_id: w.app_id.unwrap_or_default(),
                        title: w.title.unwrap_or_default(),
                        pid: w.pid,
                    })
                    .collect())
            }
            Backend::Sway => {
                let path = env::var_os("SWAYSOCK").context("SWAYSOCK is not set")?;
                let mut socket = UnixStream::connect(path).await?;
                sway_write(&mut socket, 4, b"").await?;
                let (kind, tree) = sway_read(&mut socket).await?;
                if kind != 4 {
                    bail!("unexpected Sway window inventory reply");
                }
                fn collect(node: &Value, windows: &mut Vec<Window>) {
                    if node["app_id"].is_string() || node["window_properties"].is_object() {
                        windows.push(Window {
                            app_id: node["app_id"]
                                .as_str()
                                .or_else(|| node["window_properties"]["class"].as_str())
                                .unwrap_or_default()
                                .into(),
                            title: node["name"].as_str().unwrap_or_default().into(),
                            pid: node["pid"].as_u64().and_then(|pid| pid.try_into().ok()),
                        });
                    }
                    for key in ["nodes", "floating_nodes"] {
                        if let Some(children) = node[key].as_array() {
                            for child in children {
                                collect(child, windows);
                            }
                        }
                    }
                }
                let mut windows = Vec::new();
                collect(&tree, &mut windows);
                Ok(windows)
            }
            Backend::Hyprland => {
                let mut socket = UnixStream::connect(hypr_dir()?.join(".socket.sock")).await?;
                socket.write_all(b"j/clients").await?;
                let mut bytes = Vec::new();
                (&mut socket)
                    .take(MAX_MESSAGE as u64 + 1)
                    .read_to_end(&mut bytes)
                    .await?;
                if bytes.len() > MAX_MESSAGE {
                    bail!("Hyprland window inventory exceeds size limit");
                }
                let clients: Vec<Value> = serde_json::from_slice(&bytes)?;
                Ok(clients
                    .into_iter()
                    .map(|window| Window {
                        app_id: window["class"].as_str().unwrap_or_default().into(),
                        title: window["title"].as_str().unwrap_or_default().into(),
                        pid: window["pid"].as_u64().and_then(|pid| pid.try_into().ok()),
                    })
                    .collect())
            }
            Backend::Auto => unreachable!(),
        }
    })
    .await
    .context("compositor window inventory timed out")?
}

pub async fn run(backend: Backend, tx: watch::Sender<Focus>) {
    loop {
        let result = match backend {
            Backend::Niri => niri(&tx).await,
            Backend::Sway => sway(&tx).await,
            Backend::Hyprland => hyprland(&tx).await,
            Backend::Auto => unreachable!("resolve backend before starting"),
        };
        tx.send_replace(Focus::default());
        if let Err(error) = result {
            tracing::warn!(%error, ?backend, "compositor disconnected; retrying in two seconds");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn publish(tx: &watch::Sender<Focus>, window: Option<Window>) {
    tx.send_if_modified(|state| {
        if state.connected && state.window == window {
            return false;
        }
        *state = Focus {
            connected: true,
            window,
        };
        true
    });
}

#[derive(Debug, Deserialize)]
struct NiriWindow {
    id: u64,
    title: Option<String>,
    app_id: Option<String>,
    pid: Option<u32>,
    is_focused: bool,
}

#[derive(Default)]
struct NiriState {
    windows: HashMap<u64, NiriWindow>,
    focused: Option<u64>,
}

impl NiriState {
    // Unknown variants are expected: Niri adds events without an IPC version bump.
    fn update(&mut self, event: Value) -> Result<bool> {
        if let Some(changed) = event.get("WindowsChanged") {
            let windows: Vec<NiriWindow> = serde_json::from_value(changed["windows"].clone())?;
            self.focused = windows.iter().find(|w| w.is_focused).map(|w| w.id);
            self.windows = windows.into_iter().map(|w| (w.id, w)).collect();
        } else if let Some(changed) = event.get("WindowOpenedOrChanged") {
            let window: NiriWindow = serde_json::from_value(changed["window"].clone())?;
            if window.is_focused {
                self.focused = Some(window.id);
            } else if self.focused == Some(window.id) {
                self.focused = None;
            }
            self.windows.insert(window.id, window);
        } else if let Some(changed) = event.get("WindowFocusChanged") {
            self.focused = serde_json::from_value(changed["id"].clone())?;
        } else if let Some(changed) = event.get("WindowClosed") {
            let id: u64 = serde_json::from_value(changed["id"].clone())?;
            self.windows.remove(&id);
            if self.focused == Some(id) {
                self.focused = None;
            }
        } else {
            return Ok(false);
        }
        Ok(true)
    }

    fn window(&self) -> Option<Window> {
        let window = self.windows.get(&self.focused?)?;
        Some(Window {
            app_id: window.app_id.clone().unwrap_or_default(),
            title: window.title.clone().unwrap_or_default(),
            pid: window.pid,
        })
    }
}

async fn niri(tx: &watch::Sender<Focus>) -> Result<()> {
    let path = env::var_os("NIRI_SOCKET").context("NIRI_SOCKET is not set")?;
    let mut socket = UnixStream::connect(path)
        .await
        .context("connecting to Niri")?;
    socket.write_all(b"\"EventStream\"\n").await?;
    let mut lines = FramedRead::new(socket, LinesCodec::new_with_max_length(MAX_MESSAGE));
    let reply = tokio::time::timeout(Duration::from_secs(5), lines.next())
        .await?
        .context("Niri closed before subscription acknowledgement")??;
    let reply: Value = serde_json::from_str(&reply)?;
    if reply != json!({"Ok": "Handled"}) {
        bail!("Niri rejected event stream: {reply}");
    }
    let mut state = NiriState::default();
    // EventStream starts with an atomic WindowsChanged snapshot. Do not race it with Windows.
    while let Some(line) = lines.next().await {
        if state.update(serde_json::from_str(&line?)?)? {
            publish(tx, state.window());
        }
    }
    bail!("Niri event stream ended")
}

async fn sway_write(socket: &mut UnixStream, kind: u32, payload: &[u8]) -> Result<()> {
    socket.write_all(b"i3-ipc").await?;
    socket
        .write_all(&(payload.len() as u32).to_le_bytes())
        .await?;
    socket.write_all(&kind.to_le_bytes()).await?;
    socket.write_all(payload).await?;
    Ok(())
}

async fn sway_read(socket: &mut UnixStream) -> Result<(u32, Value)> {
    let mut header = [0; 14];
    socket.read_exact(&mut header).await?;
    if &header[..6] != b"i3-ipc" {
        bail!("invalid Sway IPC header");
    }
    let length = u32::from_le_bytes(header[6..10].try_into()?) as usize;
    if length > MAX_MESSAGE {
        bail!("Sway IPC message exceeds size limit");
    }
    let kind = u32::from_le_bytes(header[10..14].try_into()?);
    let mut payload = vec![0; length];
    socket.read_exact(&mut payload).await?;
    Ok((kind, serde_json::from_slice(&payload)?))
}

fn sway_focused(node: &Value) -> Option<Window> {
    if node["focused"].as_bool() == Some(true)
        && (node["app_id"].is_string() || node["window_properties"].is_object())
    {
        return Some(Window {
            app_id: node["app_id"]
                .as_str()
                .or_else(|| node["window_properties"]["class"].as_str())
                .unwrap_or_default()
                .into(),
            title: node["name"].as_str().unwrap_or_default().into(),
            pid: node["pid"].as_u64().and_then(|pid| pid.try_into().ok()),
        });
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node[key].as_array() {
            for child in children {
                if let Some(window) = sway_focused(child) {
                    return Some(window);
                }
            }
        }
    }
    None
}

async fn sway(tx: &watch::Sender<Focus>) -> Result<()> {
    let path = env::var_os("SWAYSOCK").context("SWAYSOCK is not set")?;
    let mut events = UnixStream::connect(&path).await?;
    sway_write(&mut events, 2, b"[\"window\",\"workspace\"]").await?;
    let (kind, reply) =
        tokio::time::timeout(Duration::from_secs(5), sway_read(&mut events)).await??;
    if kind != 2 || reply["success"] != true {
        bail!("Sway rejected subscription: {reply}");
    }
    let mut queries = UnixStream::connect(&path).await?;
    loop {
        sway_write(&mut queries, 4, b"").await?;
        let (kind, tree) =
            tokio::time::timeout(Duration::from_secs(5), sway_read(&mut queries)).await??;
        if kind != 4 {
            bail!("unexpected Sway tree reply: {kind}");
        }
        publish(tx, sway_focused(&tree));
        sway_read(&mut events).await?;
    }
}

fn hypr_dir() -> Result<PathBuf> {
    let runtime = env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    let signature = env::var_os("HYPRLAND_INSTANCE_SIGNATURE")
        .context("HYPRLAND_INSTANCE_SIGNATURE is not set")?;
    Ok(PathBuf::from(runtime).join("hypr").join(signature))
}

async fn hypr_window(dir: &std::path::Path) -> Result<Option<Window>> {
    let mut socket = UnixStream::connect(dir.join(".socket.sock")).await?;
    socket.write_all(b"j/activewindow").await?;
    let mut response = Vec::new();
    (&mut socket)
        .take(MAX_MESSAGE as u64 + 1)
        .read_to_end(&mut response)
        .await?;
    if response.len() > MAX_MESSAGE {
        bail!("Hyprland reply exceeds size limit");
    }
    let window: Value = serde_json::from_slice(&response)?;
    if window["address"]
        .as_str()
        .is_none_or(|address| address.is_empty() || address == "0x0")
    {
        return Ok(None);
    }
    Ok(Some(Window {
        app_id: window["class"].as_str().unwrap_or_default().into(),
        title: window["title"].as_str().unwrap_or_default().into(),
        pid: window["pid"].as_u64().and_then(|pid| pid.try_into().ok()),
    }))
}

async fn hyprland(tx: &watch::Sender<Focus>) -> Result<()> {
    let dir = hypr_dir()?;
    let events = UnixStream::connect(dir.join(".socket2.sock")).await?;
    let mut lines = FramedRead::new(events, LinesCodec::new_with_max_length(MAX_MESSAGE));
    publish(
        tx,
        tokio::time::timeout(Duration::from_secs(5), hypr_window(&dir)).await??,
    );
    while let Some(line) = lines.next().await {
        let line = line?;
        let kind = line
            .split_once(">>")
            .map(|(kind, _)| kind)
            .unwrap_or_default();
        if matches!(
            kind,
            "activewindow"
                | "activewindowv2"
                | "windowtitle"
                | "windowtitlev2"
                | "closewindow"
                | "openwindow"
        ) {
            publish(
                tx,
                tokio::time::timeout(Duration::from_secs(5), hypr_window(&dir)).await??,
            );
        }
    }
    bail!("Hyprland event stream ended")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: u64, focused: bool, app: &str) -> Value {
        json!({"id": id, "is_focused": focused, "title": null, "app_id": app, "pid": null})
    }

    #[test]
    fn niri_snapshot_focus_and_destruction_do_not_leave_stale_game() {
        let mut state = NiriState::default();
        state.update(json!({"WindowsChanged": {"windows": [window(1, true, "game"), window(2, false, "chat")]}})).unwrap();
        assert_eq!(state.window().unwrap().app_id, "game");
        state
            .update(json!({"WindowFocusChanged": {"id": 2}}))
            .unwrap();
        assert_eq!(state.window().unwrap().app_id, "chat");
        state.update(json!({"WindowClosed": {"id": 2}})).unwrap();
        assert_eq!(state.window(), None);
        state
            .update(json!({"WindowsChanged": {"windows": [window(3, true, "new")]}}))
            .unwrap();
        state
            .update(json!({"WindowFocusChanged": {"id": 1}}))
            .unwrap();
        assert_eq!(state.window(), None);
    }

    #[test]
    fn niri_metadata_updates_and_layer_focus_are_observable() {
        let mut state = NiriState::default();
        state
            .update(json!({"WindowOpenedOrChanged": {"window": window(7, true, "steam")}}))
            .unwrap();
        state
            .update(json!({"WindowOpenedOrChanged": {"window": window(7, true, "steam_app_730")}}))
            .unwrap();
        assert_eq!(state.window().unwrap().app_id, "steam_app_730");
        assert!(!state.update(json!({"FutureEvent": {}})).unwrap());
        state
            .update(json!({"WindowFocusChanged": {"id": null}}))
            .unwrap();
        assert_eq!(state.window(), None);
    }

    #[test]
    fn sway_floating_xwayland_uses_wm_class() {
        let tree = json!({"nodes": [], "floating_nodes": [{"nodes": [{"focused": true, "app_id": null, "window_properties": {"class": "game.exe"}, "pid": 42, "name": "Game"}]}]});
        assert_eq!(
            sway_focused(&tree),
            Some(Window {
                app_id: "game.exe".into(),
                title: "Game".into(),
                pid: Some(42)
            })
        );
        assert_eq!(
            sway_focused(&json!({"focused": true, "type": "workspace"})),
            None
        );
    }
}
