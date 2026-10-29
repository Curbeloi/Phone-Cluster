//! Proxy OpenAI: elige un nodo con `policy::select` y reenvía la petición a
//! su `llama-server` sin reinterpretar el JSON (solo lee `model` y `stream`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use pc_proto::NodeId;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{ApiError, AppState, Scope, authorize, now_ms};
use crate::events::RequestDone;
use crate::policy::{self, RequestInfo};
use crate::registry::Registry;

const NON_STREAM_TIMEOUT: Duration = Duration::from_secs(300);

pub async fn models(State(s): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = authorize(&s, &headers, Scope::Inference) {
        return e.into_response();
    }
    let data: Vec<Value> = s
        .registry
        .models()
        .into_iter()
        .map(|id| json!({ "id": id, "object": "model", "created": 0, "owned_by": "phone-cluster" }))
        .collect();
    Json(json!({ "object": "list", "data": data })).into_response()
}

pub async fn chat_completions(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    forward(s, headers, body, "/v1/chat/completions").await
}

pub async fn completions(State(s): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    forward(s, headers, body, "/v1/completions").await
}

/// Los únicos campos que el coordinador mira del cuerpo.
#[derive(Debug, Default, Deserialize)]
struct Peek {
    model: Option<String>,
    #[serde(default)]
    stream: bool,
}

async fn forward(s: AppState, headers: HeaderMap, body: Bytes, endpoint: &'static str) -> Response {
    if let Err(e) = authorize(&s, &headers, Scope::Inference) {
        return e.into_response();
    }
    let peek: Peek = serde_json::from_slice(&body).unwrap_or_default();
    let started = Instant::now();
    let mut tried: Vec<NodeId> = Vec::new();
    let mut last_5xx: Option<Failed> = None;
    let mut blocked_by_macos = false;

    // Un intento más `max_retries` reintentos, cada uno en un nodo distinto.
    for _ in 0..=s.cfg.max_retries {
        let candidates = s.registry.candidates(Instant::now());
        let req = RequestInfo {
            model: peek.model.as_deref(),
            exclude: &tried,
        };
        let Some(id) = policy::select(&candidates, &req) else {
            break;
        };
        tried.push(id);
        let Some(flight) = Flight::begin(s.registry.clone(), id, endpoint, peek.stream, started)
        else {
            continue;
        };
        let url = format!("{}{endpoint}", flight.url.trim_end_matches('/'));
        let mut upstream = s.client.post(&url).body(body.clone());
        if !peek.stream {
            upstream = upstream.timeout(NON_STREAM_TIMEOUT);
        }
        for name in [header::CONTENT_TYPE, header::ACCEPT] {
            if let Some(v) = headers.get(&name) {
                upstream = upstream.header(name, v.clone());
            }
        }
        match upstream.send().await {
            Err(e) => {
                blocked_by_macos |= local_network_denied(&e);
                tracing::warn!(node = %flight.name, error = %error_chain(&e), "nodo inalcanzable; se prueba otro");
            }
            Ok(resp) if resp.status().is_server_error() => {
                let status = resp.status();
                tracing::warn!(node = %flight.name, %status, "el nodo respondió 5xx; se prueba otro");
                last_5xx = Some(Failed {
                    id,
                    name: flight.name.clone(),
                    status,
                    content_type: resp.headers().get(header::CONTENT_TYPE).cloned(),
                    body: resp.bytes().await.unwrap_or_default(),
                });
            }
            Ok(resp) => return respond(flight, resp, peek.stream).await,
        }
    }

    if let Some(f) = last_5xx {
        let now = Instant::now();
        s.registry.record(
            RequestDone {
                node_id: f.id,
                node_name: f.name.clone(),
                endpoint,
                stream: peek.stream,
                status: f.status.as_u16(),
                duration_ms: millis(now.duration_since(started)),
                tokens: None,
                tok_s: None,
                at_ms: now_ms(),
            },
            now,
        );
        let mut res = Response::new(Body::from(f.body));
        *res.status_mut() = f.status;
        if let Some(ct) = f.content_type {
            res.headers_mut().insert(header::CONTENT_TYPE, ct);
        }
        insert_node_headers(res.headers_mut(), f.id, &f.name);
        return res;
    }
    if tried.is_empty() {
        tracing::warn!(model = ?peek.model, "sin nodos disponibles");
        return ApiError::no_nodes().into_response();
    }
    if blocked_by_macos {
        return ApiError::upstream(
            "macOS está bloqueando la conexión con los teléfonos. Abre Ajustes del Sistema → \
             Privacidad y seguridad → Red local, activa Phone Cluster y vuelve a abrir la app.",
        )
        .into_response();
    }
    ApiError::upstream(format!(
        "ninguno de los {} nodos probados respondió",
        tried.len()
    ))
    .into_response()
}

/// Error y sus causas en una línea: "error sending request" solo no ayuda.
fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        out.push_str(": ");
        out.push_str(&c.to_string());
        cur = c.source();
    }
    out
}

/// macOS (15+) niega las conexiones a la red local de una app sin el permiso
/// «Red local» con `EHOSTUNREACH` («No route to host»), aunque haya ruta.
fn local_network_denied(e: &(dyn std::error::Error + 'static)) -> bool {
    const EHOSTUNREACH_MACOS: i32 = 65;
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(c) = cur {
        if let Some(io) = c.downcast_ref::<std::io::Error>()
            && cfg!(target_os = "macos")
            && io.raw_os_error() == Some(EHOSTUNREACH_MACOS)
        {
            return true;
        }
        cur = c.source();
    }
    false
}

/// Respuesta 5xx de un intento, por si se agotan los reintentos.
struct Failed {
    id: NodeId,
    name: String,
    status: StatusCode,
    content_type: Option<HeaderValue>,
    body: Bytes,
}

/// Devuelve la respuesta del nodo. En streaming los bytes pasan tal cual,
/// sin bufferizar; un error a mitad de stream corta la conexión.
async fn respond(mut flight: Flight, resp: reqwest::Response, stream: bool) -> Response {
    let status = resp.status();
    let mut out = HeaderMap::new();
    for name in [header::CONTENT_TYPE, header::CACHE_CONTROL] {
        if let Some(v) = resp.headers().get(&name) {
            out.insert(name, v.clone());
        }
    }
    insert_node_headers(&mut out, flight.id, &flight.name);
    flight.status = Some(status.as_u16());

    let body = if stream {
        let mut scan = SseScan::default();
        Body::from_stream(resp.bytes_stream().map(move |chunk| {
            if let Ok(bytes) = &chunk {
                for v in scan.push(bytes) {
                    flight.observe_json(&v);
                }
            }
            chunk
        }))
    } else {
        match resp.bytes().await {
            Ok(bytes) => {
                if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                    flight.observe_json(&v);
                }
                Body::from(bytes)
            }
            Err(e) => {
                tracing::warn!(node = %flight.name, error = %e, "respuesta cortada");
                flight.status = None;
                return ApiError::upstream(format!("{} cortó la respuesta", flight.name))
                    .into_response();
            }
        }
    };
    let mut res = Response::new(body);
    *res.status_mut() = status;
    *res.headers_mut() = out;
    res
}

/// `X-PC-Node` lleva el nombre del nodo (o su id si no es ASCII visible).
fn insert_node_headers(headers: &mut HeaderMap, id: NodeId, name: &str) {
    let id_value = HeaderValue::from_str(&id.to_string()).ok();
    let name_value = HeaderValue::from_str(name)
        .ok()
        .or_else(|| id_value.clone());
    if let Some(v) = name_value {
        headers.insert("x-pc-node", v);
    }
    if let Some(v) = id_value {
        headers.insert("x-pc-node-id", v);
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Observa un stream SSE sin alterarlo y extrae los JSON de las líneas
/// `data:` que traen `usage` o `timings` (llama-server los pone en el último
/// chunk).
#[derive(Default)]
struct SseScan {
    buf: Vec<u8>,
}

impl SseScan {
    const MAX_LINE: usize = 1 << 20;

    fn push(&mut self, chunk: &[u8]) -> Vec<Value> {
        self.buf.extend_from_slice(chunk);
        let mut found = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let Some(data) = line.trim_ascii().strip_prefix(b"data:") else {
                continue;
            };
            let interesting = contains(data, b"\"timings\"") || contains(data, b"\"usage\"");
            if interesting && let Ok(v) = serde_json::from_slice(data.trim_ascii()) {
                found.push(v);
            }
        }
        if self.buf.len() > Self::MAX_LINE {
            self.buf.clear();
        }
        found
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Una petición en curso hacia un nodo. Mantiene `in_flight` y, al soltarse,
/// registra duración, tokens y tok/s.
struct Flight {
    registry: Arc<Registry>,
    id: NodeId,
    name: String,
    url: String,
    endpoint: &'static str,
    stream: bool,
    started: Instant,
    /// `Some` cuando hubo respuesta que entregar al cliente.
    status: Option<u16>,
    tokens: Option<u64>,
    tok_s: Option<f64>,
}

impl Flight {
    fn begin(
        registry: Arc<Registry>,
        id: NodeId,
        endpoint: &'static str,
        stream: bool,
        started: Instant,
    ) -> Option<Self> {
        let target = registry.begin(id, Instant::now())?;
        Some(Self {
            registry,
            id,
            name: target.name,
            url: target.url,
            endpoint,
            stream,
            started,
            status: None,
            tokens: None,
            tok_s: None,
        })
    }

    /// Lee `usage` y `timings` si el `llama-server` los devuelve.
    fn observe_json(&mut self, v: &Value) {
        let tokens = v
            .pointer("/usage/completion_tokens")
            .or_else(|| v.pointer("/timings/predicted_n"))
            .and_then(Value::as_u64);
        let tok_s = v
            .pointer("/timings/predicted_per_second")
            .and_then(Value::as_f64)
            .filter(|x| x.is_finite() && *x > 0.0);
        self.tokens = tokens.or(self.tokens);
        self.tok_s = tok_s.or(self.tok_s);
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        let now = Instant::now();
        self.registry.end(self.id, now);
        let Some(status) = self.status else { return };
        let duration_ms = millis(now.duration_since(self.started));
        tracing::info!(
            node = %self.name,
            endpoint = self.endpoint,
            stream = self.stream,
            status,
            duration_ms,
            tokens = ?self.tokens,
            tok_s = ?self.tok_s.map(|x| (x * 10.0).round() / 10.0),
            "petición atendida"
        );
        self.registry.record(
            RequestDone {
                node_id: self.id,
                node_name: self.name.clone(),
                endpoint: self.endpoint,
                stream: self.stream,
                status,
                duration_ms,
                tokens: self.tokens,
                tok_s: self.tok_s,
                at_ms: now_ms(),
            },
            now,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_scan_finds_timings_across_chunk_boundaries() {
        let mut scan = SseScan::default();
        assert!(
            scan.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n")
                .is_empty()
        );
        assert!(scan.push(b"data: {\"choices\":[],\"timi").is_empty());
        let found = scan
            .push(b"ngs\":{\"predicted_n\":7,\"predicted_per_second\":12.5}}\n\ndata: [DONE]\n\n");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["timings"]["predicted_n"], 7);
        assert!(scan.buf.is_empty());
    }

    #[test]
    fn detects_macos_local_network_denial() {
        let denied = std::io::Error::from_raw_os_error(65);
        assert_eq!(local_network_denied(&denied), cfg!(target_os = "macos"));
        let refused = std::io::Error::from_raw_os_error(61);
        assert!(!local_network_denied(&refused));
        assert!(error_chain(&denied).contains("route"));
    }

    #[test]
    fn node_headers_fall_back_to_id() {
        let mut h = HeaderMap::new();
        let id = NodeId::from_u128(7);
        insert_node_headers(&mut h, id, "teléfono\n");
        assert_eq!(h["x-pc-node"], id.to_string());
        insert_node_headers(&mut h, id, "pixel-7");
        assert_eq!(h["x-pc-node"], "pixel-7");
    }
}
