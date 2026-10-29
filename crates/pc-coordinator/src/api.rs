//! Rutas HTTP: heartbeats, administración, SSE de eventos, OpenAI y dashboard.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::Stream;
use pc_proto::{CommandKind, Heartbeat, HeartbeatHeader, HeartbeatResponse, NodeId, PROTO_VERSION};
use serde::Deserialize;
use serde_json::json;
use tower_http::trace::{DefaultOnFailure, TraceLayer};
use tracing::Level;

use crate::config::Config;
use crate::devices::Devices;
use crate::join::{JoinRequest, JoinState, Joins};
use crate::proxy;
use crate::registry::{ForgetError, Registry};
use crate::split::{Split, SplitView};

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<Registry>,
    pub cfg: Arc<Config>,
    pub client: reqwest::Client,
    /// Listener del dashboard (loopback): administración sin token.
    pub trusted: bool,
    /// Dirección de escucha de la API; la URL pública se calcula a partir de ella.
    pub api_addr: std::net::SocketAddr,
    pub devices: Arc<Devices>,
    pub joins: Arc<Joins>,
    pub split: Arc<Split>,
}

impl AppState {
    /// `http://<IP del Mac>:<puerto>`: lo que usan los teléfonos.
    pub fn public_url(&self) -> String {
        crate::lan_base_url(self.api_addr)
    }
}

/// Error con el formato de OpenAI: `{"error":{"message","type"}}`.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub kind: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
        }
    }
    pub fn no_nodes() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_nodes_available",
            "no hay nodos disponibles para este modelo",
        )
    }
    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "upstream_unreachable", message)
    }
    fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "token ausente o incorrecto",
        )
    }
    fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "nodo desconocido")
    }
    fn unprocessable(kind: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, kind, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "error": { "message": self.message, "type": self.kind } });
        (self.status, Json(body)).into_response()
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Scope {
    Agent,
    Admin,
    Inference,
}

pub fn authorize(s: &AppState, headers: &HeaderMap, scope: Scope) -> Result<(), ApiError> {
    let required = match scope {
        Scope::Agent => true,
        Scope::Admin => !s.trusted,
        Scope::Inference => !s.trusted && s.cfg.require_token_for_inference,
    };
    if !required {
        return Ok(());
    }
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if constant_time_eq(given.as_bytes(), s.cfg.token.expose().as_bytes()) {
        Ok(())
    } else {
        Err(ApiError::unauthorized())
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn router(state: AppState) -> Router {
    let mut r = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/heartbeat", post(heartbeat))
        .route("/api/v1/nodes", get(nodes))
        .route("/api/v1/events", get(events))
        .route("/api/v1/nodes/{id}/drain", post(drain))
        .route("/api/v1/nodes/{id}/restart_llama", post(restart_llama))
        .route("/api/v1/nodes/{id}", axum::routing::delete(forget))
        .route("/api/v1/devices", get(devices))
        .route("/api/v1/devices/pair", post(pair_device))
        .route("/api/v1/devices/reinstall", post(reinstall_device))
        .route("/api/v1/split", get(split_view))
        .route("/api/v1/split/{action}", post(split_set))
        .route("/api/v1/join", post(join))
        .route("/api/v1/joins", get(joins))
        .route("/api/v1/joins/{device_id}/{decision}", post(decide_join))
        .route("/api/v1/files/model", get(model_file))
        .route("/api/v1/app-qr.svg", get(app_qr))
        .route("/app", get(app_apk))
        .route("/join", get(join_page))
        .route("/v1/models", get(proxy::models))
        .route("/v1/chat/completions", post(proxy::chat_completions))
        .route("/v1/completions", post(proxy::completions));
    if state.trusted {
        r = r
            .layer(axum::middleware::from_fn(local_only))
            .route("/", get(web_index))
            .route("/app.js", get(web_js))
            .route("/style.css", get(web_css));
    }
    r.layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .layer(
            // Los 5xx de los nodos ya se registran en el proxy con su contexto.
            TraceLayer::new_for_http().on_failure(DefaultOnFailure::new().level(Level::DEBUG)),
        )
        .with_state(state)
}

/// El dashboard no pide token porque solo escucha en loopback. Aun así, una web
/// abierta en el navegador del Mac podría llamarlo (CSRF) o apuntar su dominio
/// a 127.0.0.1 (DNS rebinding) para aceptar un teléfono suyo y llevarse el
/// token. Se exige `Host` local y, en peticiones que cambian algo, `Origin`
/// local o ausente.
async fn local_only(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if !is_local_request(req.headers(), req.method()) {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "el dashboard solo acepta peticiones de este Mac",
        )
        .into_response();
    }
    next.run(req).await
}

fn is_local_request(headers: &HeaderMap, method: &axum::http::Method) -> bool {
    let get = |name| headers.get(name).and_then(|v| v.to_str().ok());
    let host_ok = get(header::HOST).is_some_and(is_loopback_host);
    let origin_ok = get(header::ORIGIN).is_none_or(|o| {
        o.split_once("://")
            .is_some_and(|(_, h)| is_loopback_host(h))
    });
    let read_only = matches!(
        *method,
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    host_ok && (read_only || origin_ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn local_request_guard() {
        let local = headers(&[("host", "127.0.0.1:7801")]);
        assert!(is_local_request(&local, &Method::POST));
        let with_origin = headers(&[
            ("host", "127.0.0.1:7801"),
            ("origin", "http://127.0.0.1:7801"),
        ]);
        assert!(is_local_request(&with_origin, &Method::POST));
        let csrf = headers(&[
            ("host", "127.0.0.1:7801"),
            ("origin", "https://evil.example"),
        ]);
        assert!(!is_local_request(&csrf, &Method::POST));
        assert!(is_local_request(&csrf, &Method::GET), "GET no cambia nada");
        let rebinding = headers(&[("host", "evil.example:7801")]);
        assert!(!is_local_request(&rebinding, &Method::GET));
        assert!(is_local_request(
            &headers(&[("host", "[::1]:7801")]),
            &Method::GET
        ));
        assert!(is_local_request(
            &headers(&[("host", "localhost")]),
            &Method::GET
        ));
        assert!(!is_local_request(&HeaderMap::new(), &Method::GET));
    }
}

/// `127.0.0.1:7801`, `localhost`, `[::1]:7801`…
fn is_loopback_host(host_port: &str) -> bool {
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

async fn heartbeat(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<HeartbeatResponse>, ApiError> {
    authorize(&s, &headers, Scope::Agent)?;
    let header: HeartbeatHeader = serde_json::from_slice(&body)
        .map_err(|e| ApiError::unprocessable("invalid_heartbeat", e.to_string()))?;
    match header.proto_version {
        Some(PROTO_VERSION) => {}
        Some(v) => {
            let reason = format!("proto_version {v} (el coordinador habla {PROTO_VERSION})");
            tracing::warn!(name = ?header.name, %reason, "heartbeat incompatible");
            s.registry
                .incompatible(&header, reason.clone(), Instant::now());
            return Err(ApiError::unprocessable("unsupported_proto_version", reason));
        }
        None => {
            return Err(ApiError::unprocessable(
                "invalid_heartbeat",
                "falta proto_version",
            ));
        }
    }
    let hb: Heartbeat = serde_json::from_slice(&body)
        .map_err(|e| ApiError::unprocessable("invalid_heartbeat", e.to_string()))?;
    hb.validate()
        .map_err(|e| ApiError::unprocessable("invalid_heartbeat", e.to_string()))?;
    Ok(Json(s.registry.heartbeat(hb, Instant::now())))
}

async fn nodes(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&s, &headers, Scope::Admin)?;
    let now_ms = now_ms();
    Ok(Json(json!({
        "nodes": s.registry.nodes(Instant::now()),
        "recent_requests_ms": s.registry.recent_requests(now_ms),
        "now_ms": now_ms,
        "openai_base_url": format!("{}/v1", s.public_url()),
        "thresholds": {
            "max_temp_c": s.cfg.max_temp_c,
            "resume_temp_c": s.cfg.resume_temp_c,
            "max_soc_temp_c": s.cfg.max_soc_temp_c,
            "resume_soc_temp_c": s.cfg.resume_soc_temp_c,
        },
    })))
}

/// SSE. Si el cliente se queda atrás se cierra el stream: el dashboard
/// reconecta y vuelve a pedir la lista completa.
async fn events(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    authorize(&s, &headers, Scope::Admin)?;
    let rx = s.registry.events().subscribe();
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        let ev = rx.recv().await.ok()?;
        let sse = SseEvent::default().event(ev.name()).data(ev.data());
        Some((Ok(sse), rx))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

#[derive(Deserialize)]
struct DrainBody {
    drain: bool,
}

async fn drain(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<NodeId>,
    Json(body): Json<DrainBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&s, &headers, Scope::Admin)?;
    let view = s
        .registry
        .set_drain(id, body.drain, Instant::now())
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(json!(view)))
}

/// Encola `restart_llama`; el agente lo recibe con el próximo heartbeat.
async fn restart_llama(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<NodeId>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    authorize(&s, &headers, Scope::Admin)?;
    let cmd = s
        .registry
        .queue_command(id, CommandKind::RestartLlama, Instant::now())
        .ok_or_else(ApiError::not_found)?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "command": cmd }))))
}

async fn split_view(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SplitView>, ApiError> {
    authorize(&s, &headers, Scope::Admin)?;
    Ok(Json(s.split.view()))
}

/// `on` / `off`. El trabajo lo hace la tarea del modo dividido; el progreso
/// llega por SSE (`split_updated`).
async fn split_set(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(action): Path<String>,
) -> Result<Json<SplitView>, ApiError> {
    authorize(&s, &headers, Scope::Admin)?;
    let enabled = match action.as_str() {
        "on" => true,
        "off" => false,
        _ => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "usa on u off",
            ));
        }
    };
    Ok(Json(s.split.set_enabled(enabled)))
}

async fn devices(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<crate::devices::Status>, ApiError> {
    authorize(&s, &headers, Scope::Admin)?;
    Ok(Json(s.devices.status()))
}

#[derive(Deserialize)]
struct PairBody {
    key: String,
    code: String,
}

async fn pair_device(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PairBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&s, &headers, Scope::Admin)?;
    let message = s
        .devices
        .pair(&body.key, &body.code)
        .await
        .map_err(|m| ApiError::new(StatusCode::BAD_REQUEST, "pairing_failed", m))?;
    Ok(Json(json!({ "message": message })))
}

#[derive(Deserialize)]
struct DeviceKey {
    key: String,
}

async fn reinstall_device(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<DeviceKey>,
) -> Result<StatusCode, ApiError> {
    authorize(&s, &headers, Scope::Admin)?;
    if s.devices.reinstall(&body.key) {
        Ok(StatusCode::ACCEPTED)
    } else {
        Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "dispositivo desconocido",
        ))
    }
}

/// Nombre del APK que genera `tools/build-android.sh` en `artifacts_dir`.
pub const APK_FILE: &str = "phone-cluster.apk";

/// Lo que abre el QR: una página para el teléfono con «Descargar» y «Conectar».
fn app_url(s: &AppState) -> String {
    format!("{}/join", s.public_url())
}

const JOIN_HTML: &str = include_str!("../web/join.html");

async fn join_page() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        JOIN_HTML,
    )
}

/// La app Android pide unirse (sin token: aún no lo tiene).
async fn join(
    State(s): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    Json(req): Json<JoinRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    req.validate()
        .map_err(|m| ApiError::unprocessable("invalid_join", m))?;
    let token_ok = req
        .token
        .as_deref()
        .is_some_and(|t| constant_time_eq(t.as_bytes(), s.cfg.token.expose().as_bytes()));
    let view = s
        .joins
        .request(&req, peer.ip(), token_ok, Instant::now())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too_many_joins",
                "demasiadas solicitudes pendientes",
            )
        })?;
    let mut body = json!({ "status": view.state });
    if view.state == JoinState::Approved {
        let model = crate::devices::find_model(&s.cfg.adb.artifacts_dir);
        let file = model
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned());
        let size = model
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len());
        let alias = file.as_deref().map(|f| s.cfg.adb.alias_for(f));
        body["config"] = json!({
            "name": view.node_name,
            "token": s.cfg.token.expose(),
            "coordinator_url": s.public_url(),
            "model_file": file,
            "model_size": size,
            "model_url": "/api/v1/files/model",
            "model_alias": alias,
            "llama_port": s.cfg.adb.llama_port,
            "llama_args": s.cfg.adb.llama_args,
        });
    }
    Ok(Json(body))
}

async fn joins(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&s, &headers, Scope::Admin)?;
    let dir = &s.cfg.adb.artifacts_dir;
    Ok(Json(json!({
        "app_ready": dir.join(APK_FILE).is_file(),
        "model_ready": crate::devices::find_model(dir).is_some(),
        "app_url": app_url(&s),
        "lan_ok": !s.public_url().contains("127.0.0.1"),
        "requests": s.joins.list(),
    })))
}

async fn decide_join(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path((device_id, decision)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&s, &headers, Scope::Admin)?;
    let approve = match decision.as_str() {
        "approve" => true,
        "reject" => false,
        _ => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "usa approve o reject",
            ));
        }
    };
    let view = s.joins.decide(&device_id, approve).ok_or_else(|| {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", "solicitud desconocida")
    })?;
    Ok(Json(json!(view)))
}

/// El modelo para los teléfonos aprobados (con soporte de `Range` para reanudar).
async fn model_file(
    State(s): State<AppState>,
    headers: HeaderMap,
    req: axum::extract::Request,
) -> Result<Response, ApiError> {
    authorize(&s, &headers, Scope::Agent)?;
    let path = crate::devices::find_model(&s.cfg.adb.artifacts_dir).ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "model_missing",
            "el Mac aún no tiene el modelo",
        )
    })?;
    serve_file(&path, req).await
}

/// El APK de la app Android, para instalarlo escaneando el QR.
async fn app_apk(State(s): State<AppState>, req: axum::extract::Request) -> Response {
    let path = s.cfg.adb.artifacts_dir.join(APK_FILE);
    if !path.is_file() {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "La app aún no está compilada en el Mac: ejecuta tools/build-android.sh",
        )
            .into_response();
    }
    match serve_file(&path, req).await {
        Ok(mut res) => {
            let h = res.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/vnd.android.package-archive"),
            );
            h.insert(
                header::CONTENT_DISPOSITION,
                axum::http::HeaderValue::from_static("attachment; filename=\"PhoneCluster.apk\""),
            );
            res
        }
        Err(e) => e.into_response(),
    }
}

async fn serve_file(
    path: &std::path::Path,
    req: axum::extract::Request,
) -> Result<Response, ApiError> {
    use tower::ServiceExt;
    let res = tower_http::services::ServeFile::new(path)
        .oneshot(req)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "io_error", e.to_string()))?;
    Ok(res.map(axum::body::Body::new))
}

/// QR con la URL de descarga del APK, para escanearlo con la cámara.
async fn app_qr(State(s): State<AppState>) -> Result<Response, ApiError> {
    let code = qrcode::QrCode::new(app_url(&s).as_bytes())
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "qr_error", e.to_string()))?;
    let svg = code
        .render::<qrcode::render::svg::Color<'_>>()
        .min_dimensions(220, 220)
        .quiet_zone(true)
        .build();
    Ok(([(header::CONTENT_TYPE, "image/svg+xml")], svg).into_response())
}

async fn forget(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<NodeId>,
) -> Result<StatusCode, ApiError> {
    authorize(&s, &headers, Scope::Admin)?;
    match s.registry.forget(id, Instant::now()) {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(ForgetError::NotFound) => Err(ApiError::not_found()),
        Err(ForgetError::NotOffline) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "node_not_offline",
            "solo se puede olvidar un nodo Offline",
        )),
    }
}

pub fn now_ms() -> i64 {
    i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const STYLE_CSS: &str = include_str!("../web/style.css");

async fn web_index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        INDEX_HTML,
    )
}

async fn web_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        APP_JS,
    )
}

async fn web_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLE_CSS,
    )
}
