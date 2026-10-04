//! Loopback-only gRPC-Web facade for the Wootility browser client.
use std::{
    convert::Infallible,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::Response,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use prost::Message;
use tokio::{net::TcpListener, sync::watch};

use crate::{
    applications, compositor, config,
    metrics::{DataPoint, DataValue},
    proto,
};

const MAX_BODY: usize = 1024 * 1024;
const ALLOWED_ORIGINS: [&str; 3] = [
    "https://wootility.io",
    "https://beta.wootility.io",
    "https://v5.wootility.io",
];
const UPDATE_MESSAGE: &str = "No update feed is available; upgrade open-wbg-service with your package manager or run cargo install --path . --locked --force from its source checkout.";

#[derive(Clone)]
pub struct ApiState {
    pub settings: Arc<Mutex<config::Store>>,
    pub backend: compositor::Backend,
    pub points: watch::Receiver<Vec<DataPoint>>,
    pub status: watch::Receiver<serde_json::Value>,
    pub inventory_changed: watch::Receiver<u64>,
    pub log_dir: PathBuf,
}

struct Server {
    state: ApiState,
    shutdown: watch::Receiver<bool>,
    apps: Mutex<Option<proto::Apps>>,
}

pub async fn serve(
    listener: TcpListener,
    state: ApiState,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    ensure!(
        listener.local_addr()?.ip().is_loopback(),
        "Wootility API must listen on loopback only"
    );
    let state = Arc::new(Server {
        state,
        shutdown: shutdown.clone(),
        apps: Mutex::new(None),
    });
    let router = Router::new().fallback(dispatch).with_state(state);
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let mut shutdown = shutdown;
            if !*shutdown.borrow() {
                while shutdown.changed().await.is_ok() {
                    if *shutdown.borrow_and_update() {
                        break;
                    }
                }
            }
        })
        .await
        .context("Wootility API listener failed")
}

// Host verification is performed even for requests without Origin: native clients are
// allowed, but DNS rebinding and untrusted browser origins must not gain local privileges.
fn trusted_request(
    headers: &HeaderMap,
    port: u16,
) -> std::result::Result<Option<&str>, StatusCode> {
    let mut hosts = headers.get_all(header::HOST).iter();
    let host = hosts
        .next()
        .and_then(|h| h.to_str().ok())
        .ok_or(StatusCode::FORBIDDEN)?;
    let valid_host = host.rsplit_once(':').is_some_and(|(name, service)| {
        (name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" || name == "[::1]")
            && service.parse::<u16>() == Ok(port)
    });
    if hosts.next().is_some() || !valid_host {
        return Err(StatusCode::FORBIDDEN);
    }
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let origin = origins
        .next()
        .map(|h| h.to_str().map_err(|_| StatusCode::FORBIDDEN))
        .transpose()?;
    if origins.next().is_some() || origin.is_some_and(|o| !ALLOWED_ORIGINS.contains(&o)) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(origin)
}

fn reply(
    status: StatusCode,
    body: Body,
    content_type: &'static str,
    origin: Option<&str>,
) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        content_type.parse().expect("static content type"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static cache control"),
    );
    headers.insert(header::VARY, "Origin".parse().expect("static vary"));
    if let Some(origin) = origin {
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            origin.parse().expect("trusted origin"),
        );
    }
    response
}

async fn dispatch(State(server): State<Arc<Server>>, req: Request) -> Response {
    let port = match server.state.settings.lock() {
        Ok(settings) => settings.tx.borrow().api_port,
        Err(_) => {
            return reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                Body::empty(),
                "text/plain",
                None,
            );
        }
    };
    let origin = match trusted_request(req.headers(), port) {
        Ok(origin) => origin.map(str::to_owned),
        Err(status) => return reply(status, Body::empty(), "text/plain", None),
    };
    let origin = origin.as_deref();
    if req.method() == Method::OPTIONS {
        return preflight(req.uri().path(), req.headers(), origin);
    }
    if req.method() == Method::GET && req.uri().path() == "/status" {
        let status = server.state.status.borrow().clone();
        return reply(
            StatusCode::OK,
            Body::from(status.to_string()),
            "application/json",
            origin,
        );
    }
    if req.method() != Method::POST {
        return reply(
            StatusCode::METHOD_NOT_ALLOWED,
            Body::empty(),
            "text/plain",
            origin,
        );
    }
    let mut content_types = req.headers().get_all(header::CONTENT_TYPE).iter();
    let text = match content_types.next().and_then(|v| v.to_str().ok()) {
        // Protobuf is the default codec when the optional +proto suffix is absent.
        Some("application/grpc-web" | "application/grpc-web+proto")
            if content_types.next().is_none() =>
        {
            false
        }
        Some("application/grpc-web-text" | "application/grpc-web-text+proto")
            if content_types.next().is_none() =>
        {
            true
        }
        _ => {
            return reply(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                Body::empty(),
                "text/plain",
                origin,
            );
        }
    };
    let path = req.uri().path().to_owned();
    let bytes = match to_bytes(req.into_body(), MAX_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return grpc_error(
                8,
                "Request exceeds 1 MiB or has an invalid body",
                text,
                origin,
            );
        }
    };
    let request = match decode_frame(bytes, text) {
        Ok(request) => request,
        Err(message) => return grpc_error(3, message, text, origin),
    };
    if path
        == "/data_source_manager_service.DataSourceManagerCommunicationService/ListenToDataPointUpdates"
    {
        if let Err(error) = proto::Empty::decode(request.as_ref()) {
            return grpc_error(
                3,
                &format!("Invalid request protobuf: {error}"),
                text,
                origin,
            );
        }
        return listen(&server, text, origin);
    }
    match invoke(&server, &path, request.as_ref()).await {
        Ok(payload) => grpc_success(&payload, text, origin),
        Err((code, message)) => grpc_error(code, &message, text, origin),
    }
}

fn preflight(path: &str, headers: &HeaderMap, origin: Option<&str>) -> Response {
    let Some(origin) = origin else {
        return reply(StatusCode::FORBIDDEN, Body::empty(), "text/plain", None);
    };
    let method = headers
        .get(header::ACCESS_CONTROL_REQUEST_METHOD)
        .and_then(|h| h.to_str().ok());
    let valid_path = path.starts_with("/wooting_service.WootilityService/")
        || path.starts_with("/data_source_manager_service.DataSourceManagerCommunicationService/")
        || path.starts_with("/app_linking.AppLinkingService/");
    if !((method == Some("GET") && path == "/status") || (method == Some("POST") && valid_path)) {
        return reply(StatusCode::FORBIDDEN, Body::empty(), "text/plain", None);
    }
    let requested = headers
        .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let allowed = [
        "content-type",
        "x-grpc-web",
        "x-user-agent",
        "grpc-timeout",
        "accept",
    ];
    if requested.split(',').any(|part| {
        !part.trim().is_empty() && !allowed.contains(&part.trim().to_ascii_lowercase().as_str())
    }) {
        return reply(StatusCode::FORBIDDEN, Body::empty(), "text/plain", None);
    }
    let pna = headers.get("access-control-request-private-network");
    if pna.is_some_and(|value| value != "true") {
        return reply(StatusCode::FORBIDDEN, Body::empty(), "text/plain", None);
    }
    let mut response = reply(
        StatusCode::NO_CONTENT,
        Body::empty(),
        "text/plain",
        Some(origin),
    );
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        method
            .expect("validated method")
            .parse()
            .expect("valid method"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        "content-type, x-grpc-web, x-user-agent, grpc-timeout, accept"
            .parse()
            .expect("static headers"),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        "600".parse().expect("static age"),
    );
    headers.insert(header::VARY, "Origin, Access-Control-Request-Method, Access-Control-Request-Headers, Access-Control-Request-Private-Network".parse().expect("static vary"));
    if pna.is_some() {
        headers.insert(
            "access-control-allow-private-network",
            "true".parse().expect("static pna"),
        );
    }
    response
}

// A unary gRPC-Web request has exactly one uncompressed data frame. Client trailers,
// compression and additional frames cannot silently change what method is invoked.
fn decode_frame(body: Bytes, text: bool) -> std::result::Result<Bytes, &'static str> {
    let decoded = if text {
        Bytes::from(
            STANDARD
                .decode(&body)
                .map_err(|_| "Invalid gRPC-Web base64")?,
        )
    } else {
        body
    };
    if decoded.len() < 5 || decoded[0] != 0 {
        return Err("Expected one uncompressed gRPC-Web data frame");
    }
    let length = u32::from_be_bytes(decoded[1..5].try_into().expect("four bytes")) as usize;
    if length > MAX_BODY || length != decoded.len() - 5 {
        return Err("Invalid gRPC-Web frame length");
    }
    Ok(decoded.slice(5..))
}

fn frame(flag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(flag);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn trailer(code: u32, message: &str) -> Vec<u8> {
    let mut trailers = format!("grpc-status: {code}\r\n");
    if !message.is_empty() {
        trailers.push_str("grpc-message: ");
        for byte in message.bytes() {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                trailers.push(byte as char);
            } else {
                trailers.push_str(&format!("%{byte:02X}"));
            }
        }
        trailers.push_str("\r\n");
    }
    frame(0x80, trailers.as_bytes())
}

fn grpc_reply(body: Body, text: bool, origin: Option<&str>) -> Response {
    let mut response = reply(
        StatusCode::OK,
        body,
        if text {
            "application/grpc-web-text+proto"
        } else {
            "application/grpc-web+proto"
        },
        origin,
    );
    response.headers_mut().insert(
        "access-control-expose-headers",
        "grpc-status, grpc-message".parse().expect("static headers"),
    );
    response
}

fn grpc_success(payload: &[u8], text: bool, origin: Option<&str>) -> Response {
    if payload.len() > MAX_BODY {
        return grpc_error(8, "Response exceeds 1 MiB", text, origin);
    }
    let mut encoded = frame(0, payload);
    encoded.extend(trailer(0, ""));
    grpc_reply(
        Body::from(if text {
            STANDARD.encode(encoded).into_bytes()
        } else {
            encoded
        }),
        text,
        origin,
    )
}

fn grpc_error(code: u32, message: &str, text: bool, origin: Option<&str>) -> Response {
    let encoded = trailer(code, message);
    grpc_reply(
        Body::from(if text {
            STANDARD.encode(encoded).into_bytes()
        } else {
            encoded
        }),
        text,
        origin,
    )
}

fn listen(server: &Server, text: bool, origin: Option<&str>) -> Response {
    let mut changed = server.state.inventory_changed.clone();
    changed.borrow_and_update();
    let mut shutdown = server.shutdown.clone();
    let stream = async_stream::stream! {
        let mut pending = Vec::new();
        while !*shutdown.borrow() {
            tokio::select! {
                result = changed.changed() => {
                    if result.is_err() { break; }
                    let message = frame(0, &proto::Empty {}.encode_to_vec());
                    if text {
                        pending.extend(message);
                        let length = pending.len() / 3 * 3;
                        if length != 0 {
                            let chunk = STANDARD.encode(&pending[..length]);
                            pending.drain(..length);
                            yield Ok::<Bytes, Infallible>(Bytes::from(chunk));
                        }
                    } else { yield Ok::<Bytes, Infallible>(Bytes::from(message)); }
                }
                result = shutdown.changed() => {
                    if result.is_err() || *shutdown.borrow_and_update() { break; }
                }
            }
        }
        let end = trailer(0, "");
        if text {
            pending.extend(end);
            yield Ok::<Bytes, Infallible>(Bytes::from(STANDARD.encode(pending)));
        } else { yield Ok::<Bytes, Infallible>(Bytes::from(end)); }
    };
    grpc_reply(Body::from_stream(stream), text, origin)
}

type RpcResult = std::result::Result<Vec<u8>, (u32, String)>;
fn rpc_error(code: u32, message: impl ToString) -> (u32, String) {
    (code, message.to_string())
}
fn parse<M: Message + Default>(bytes: &[u8]) -> std::result::Result<M, (u32, String)> {
    M::decode(bytes).map_err(|error| rpc_error(3, format!("Invalid request protobuf: {error}")))
}
fn encoded<M: Message>(message: M) -> RpcResult {
    Ok(message.encode_to_vec())
}

async fn invoke(server: &Server, path: &str, bytes: &[u8]) -> RpcResult {
    use proto::{Empty, Enabled};
    const WOOT: &str = "/wooting_service.WootilityService/";
    const DATA: &str = "/data_source_manager_service.DataSourceManagerCommunicationService/";
    const APP: &str = "/app_linking.AppLinkingService/";
    if let Some(method) = path.strip_prefix(WOOT) {
        match method {
            "Heartbeat" => {
                let _: Empty = parse(bytes)?;
                let enabled = crate::installation::enabled()
                    .await
                    .map_err(|e| rpc_error(13, e))?;
                encoded(proto::Heartbeat {
                    current_version: format!("open-wbg-service {}", env!("CARGO_PKG_VERSION")),
                    auto_start_enabled: enabled,
                })
            }
            "GetAutoStartEnabled" => {
                let _: Empty = parse(bytes)?;
                encoded(Enabled {
                    enabled: crate::installation::enabled()
                        .await
                        .map_err(|e| rpc_error(13, e))?,
                })
            }
            "SetAutoStartEnabled" => {
                let request: Enabled = parse(bytes)?;
                crate::installation::set_enabled(request.enabled)
                    .await
                    .map_err(|e| rpc_error(13, e))?;
                encoded(Empty {})
            }
            "GetAppSwitchNotificationEnabled" => {
                let _: Empty = parse(bytes)?;
                let settings = server.state.settings.lock().map_err(|e| rpc_error(13, e))?;
                let enabled = settings.tx.borrow().notify;
                encoded(Enabled { enabled })
            }
            "SetAppSwitchNotificationEnabled" => {
                let request: Enabled = parse(bytes)?;
                let settings = server.state.settings.lock().map_err(|e| rpc_error(13, e))?;
                settings
                    .update(|config| {
                        config.notify = request.enabled;
                        Ok(())
                    })
                    .map_err(|e| rpc_error(13, e))?;
                encoded(Empty {})
            }
            "OpenLogFolder" => {
                let _: Empty = parse(bytes)?;
                open_log_dir(&server.state.log_dir).await?;
                encoded(Empty {})
            }
            "CheckForUpdates" | "TriggerUpdate" => {
                let _: Empty = parse(bytes)?;
                Err(rpc_error(9, UPDATE_MESSAGE))
            }
            _ => Err(rpc_error(12, "Wootility method is not implemented")),
        }
    } else if let Some(method) = path.strip_prefix(DATA) {
        match method {
            "GetAllDataPointInfo" => {
                let _: Empty = parse(bytes)?;
                let points = server.state.points.borrow();
                encoded(proto::Points {
                    data_points: points.iter().map(point_info).collect(),
                })
            }
            "GetAllDataSources" => {
                let _: Empty = parse(bytes)?;
                let settings = server.state.settings.lock().map_err(|e| rpc_error(13, e))?;
                let sources = config::SOURCES
                    .iter()
                    .map(|(id, name)| proto::SourceInfo {
                        meta: Some(proto::SourceMetadata {
                            id: (*id).into(),
                            name: (*name).into(),
                            icon: String::new(),
                            description: String::new(),
                        }),
                        enabled: settings.tx.borrow().enabled_sources.contains(*id),
                    })
                    .collect();
                encoded(proto::Sources {
                    data_sources: sources,
                })
            }
            "SetDataSourceEnabled" => {
                let request: proto::SetSource = parse(bytes)?;
                if !config::SOURCES.iter().any(|(id, _)| *id == request.id) {
                    return Err(rpc_error(3, "Unknown data source id"));
                }
                let settings = server.state.settings.lock().map_err(|e| rpc_error(13, e))?;
                settings
                    .update(|config| {
                        if request.enabled && request.id == "discord_source" {
                            crate::discord::validate_credentials(&config.discord)?;
                        }
                        if request.enabled {
                            config.enabled_sources.insert(request.id.clone());
                        } else {
                            config.enabled_sources.remove(&request.id);
                        }
                        Ok(())
                    })
                    .map_err(|e| rpc_error(9, e))?;
                encoded(Empty {})
            }
            _ => Err(rpc_error(12, "Data source method is not implemented")),
        }
    } else if let Some(method) = path.strip_prefix(APP) {
        match method {
            "GetLinkableApps" => {
                let request: proto::AppRequest = parse(bytes)?;
                if !request.force_refresh {
                    let cached = server.apps.lock().map_err(|e| rpc_error(13, e))?;
                    if let Some(cached) = cached.as_ref() {
                        return Ok(cached.encode_to_vec());
                    }
                }
                let apps = tokio::task::spawn_blocking(applications::installed_apps)
                    .await
                    .map_err(|e| rpc_error(13, e))?;
                let apps = proto::Apps {
                    steam_apps: apps
                        .steam
                        .into_iter()
                        .map(|app| proto::SteamApp {
                            name: app.name,
                            app_id: app.id,
                        })
                        .collect(),
                    disk_apps: apps
                        .disk
                        .into_iter()
                        .map(|app| proto::DiskApp {
                            name: app.name,
                            executable: Some(app.executable),
                        })
                        .collect(),
                };
                let payload = apps.encode_to_vec();
                *server.apps.lock().map_err(|e| rpc_error(13, e))? = Some(apps);
                Ok(payload)
            }
            "GetOpenWindows" => {
                let _: Empty = parse(bytes)?;
                let windows = compositor::windows(server.state.backend)
                    .await
                    .map_err(|e| rpc_error(14, e))?;
                let windows = tokio::task::spawn_blocking(move || {
                    windows
                        .into_iter()
                        .map(|window| {
                            let identity = applications::identify(&window);
                            proto::OpenWindow {
                                title: window.title,
                                process_id: window.pid.map(u64::from).unwrap_or(0),
                                process_path: identity
                                    .executable_paths
                                    .into_iter()
                                    .next()
                                    .unwrap_or_default(),
                                app_name: identity
                                    .process_names
                                    .into_iter()
                                    .next()
                                    .unwrap_or(window.app_id),
                            }
                        })
                        .collect()
                })
                .await
                .map_err(|e| rpc_error(13, e))?;
                encoded(proto::OpenWindows {
                    open_windows: windows,
                })
            }
            _ => Err(rpc_error(12, "App linking method is not implemented")),
        }
    } else {
        Err(rpc_error(12, "Unknown gRPC method"))
    }
}

fn point_info(point: &DataPoint) -> proto::PointInfo {
    use proto::point_metadata::ValueType;
    let (value_type, value) = match &point.value {
        DataValue::Bool(value) => (
            ValueType::Boolean(proto::Empty {}),
            if *value { 1.0 } else { 0.0 },
        ),
        DataValue::Float(value) => (
            ValueType::Number(proto::NumberMetadata {
                minimum: Some(point.min),
                maximum: Some(point.max),
                unit: None,
            }),
            *value,
        ),
        DataValue::Integer(value) if point.path == "discord/notification" => {
            (ValueType::Event(proto::Empty {}), *value as f32)
        }
        DataValue::Integer(value) => (
            ValueType::Number(proto::NumberMetadata {
                minimum: Some(point.min),
                maximum: Some(point.max),
                unit: None,
            }),
            *value as f32,
        ),
    };
    let category = config::SOURCES
        .iter()
        .find(|(id, _)| *id == point.source)
        .map(|(_, name)| *name)
        .unwrap_or(&point.source);
    proto::PointInfo {
        data_point: Some(proto::PointId {
            is_internal: false,
            id: proto::point_hash(&point.path),
        }),
        meta: Some(proto::PointMetadata {
            key: point.path.clone(),
            title: point.name.clone(),
            description: String::new(),
            category: category.into(),
            hidden: false,
            value_type: Some(value_type),
        }),
        value,
    }
}

async fn open_log_dir(path: &PathBuf) -> std::result::Result<(), (u32, String)> {
    let path = std::fs::canonicalize(path)
        .map_err(|e| rpc_error(9, format!("Log directory is unavailable: {e}")))?;
    if !path.is_dir() {
        return Err(rpc_error(9, "Log directory is not a directory"));
    }
    let mut child = tokio::process::Command::new("xdg-open")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| rpc_error(14, format!("Cannot launch xdg-open: {e}")))?;
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(rpc_error(9, format!("xdg-open failed with {status}"))),
        Ok(Err(error)) => Err(rpc_error(13, format!("xdg-open failed: {error}"))),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(rpc_error(4, "xdg-open timed out"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stock_wootility_text_media_type_is_accepted() {
        let (settings, _) = watch::channel(Arc::new(config::Config {
            notify: true,
            ..config::Config::default()
        }));
        let server = Arc::new(Server {
            state: ApiState {
                settings: Arc::new(Mutex::new(config::Store {
                    path: PathBuf::new(),
                    tx: settings,
                })),
                backend: compositor::Backend::Niri,
                points: watch::channel(Vec::new()).1,
                status: watch::channel(serde_json::Value::Null).1,
                inventory_changed: watch::channel(0).1,
                log_dir: PathBuf::new(),
            },
            shutdown: watch::channel(false).1,
            apps: Mutex::new(None),
        });
        // Wootility 5.4.2's default protobuf-ts transport omits the optional
        // +proto suffix and sends this base64-encoded empty request.
        let request = Request::builder()
            .method(Method::POST)
            .uri("/wooting_service.WootilityService/GetAppSwitchNotificationEnabled")
            .header(header::HOST, "localhost:50052")
            .header(header::ORIGIN, "https://wootility.io")
            .header(header::ACCEPT, "application/grpc-web-text")
            .header(header::CONTENT_TYPE, "application/grpc-web-text")
            .header("x-grpc-web", "1")
            .body(Body::from("AAAAAAA="))
            .unwrap();
        let response = dispatch(State(server), request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://wootility.io"
        );
        let body = to_bytes(response.into_body(), MAX_BODY).await.unwrap();
        let wire = STANDARD.decode(body).unwrap();
        assert_eq!(wire[0], 0);
        let length = u32::from_be_bytes(wire[1..5].try_into().unwrap()) as usize;
        let enabled = proto::Enabled::decode(&wire[5..5 + length]).unwrap();
        assert!(enabled.enabled);
        let trailers = &wire[5 + length..];
        assert_eq!(trailers[0], 0x80);
        let length = u32::from_be_bytes(trailers[1..5].try_into().unwrap()) as usize;
        assert_eq!(trailers.len(), 5 + length);
        assert!(
            std::str::from_utf8(&trailers[5..])
                .unwrap()
                .contains("grpc-status: 0\r\n")
        );
    }

    #[test]
    fn unary_frame_is_exact_and_uncompressed() {
        let mut wire = frame(0, &[8, 1]);
        assert_eq!(
            decode_frame(Bytes::from(wire.clone()), false)
                .unwrap()
                .as_ref(),
            &[8, 1]
        );
        assert_eq!(
            decode_frame(Bytes::from(STANDARD.encode(&wire)), true)
                .unwrap()
                .as_ref(),
            &[8, 1]
        );
        wire.extend(frame(0, &[]));
        assert!(decode_frame(Bytes::from(wire), false).is_err());
        assert!(decode_frame(Bytes::from_static(&[1, 0, 0, 0, 0]), false).is_err());
        assert!(decode_frame(Bytes::from_static(&[0, 0, 0, 0, 1]), false).is_err());
        assert!(decode_frame(Bytes::from_static(&[0x80, 0, 0, 0, 0]), false).is_err());
        assert!(decode_frame(Bytes::from_static(b"%%%"), true).is_err());
    }
    #[test]
    fn only_exact_browser_origins_and_loopback_hosts() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "localhost:50052".parse().unwrap());
        assert_eq!(trusted_request(&headers, 50052).unwrap(), None);
        headers.insert(header::ORIGIN, "https://wootility.io".parse().unwrap());
        assert_eq!(
            trusted_request(&headers, 50052).unwrap(),
            Some("https://wootility.io")
        );
        for value in [
            "null",
            "https://wootility.io.evil.test",
            "http://wootility.io",
            "https://wootility.io/",
        ] {
            headers.insert(header::ORIGIN, value.parse().unwrap());
            assert!(trusted_request(&headers, 50052).is_err());
        }
        headers.insert(header::ORIGIN, "https://wootility.io".parse().unwrap());
        headers.append(header::ORIGIN, "https://wootility.io".parse().unwrap());
        assert!(trusted_request(&headers, 50052).is_err());
        headers.remove(header::ORIGIN);
        headers.insert(header::HOST, "evil.test:50052".parse().unwrap());
        assert!(trusted_request(&headers, 50052).is_err());
        headers.insert(header::HOST, "[::1]:50052".parse().unwrap());
        assert!(trusted_request(&headers, 50053).is_err());
        assert!(trusted_request(&headers, 50052).is_ok());
    }
}
