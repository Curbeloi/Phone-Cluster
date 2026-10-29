//! `llama-server` falso. Imita las respuestas de llama.cpp b11963 en
//! `/health`, `/props`, `/slots`, `/v1/models`, `/v1/chat/completions` y
//! `/v1/completions` (ver DECISIONS.md, D-05), con latencia configurable.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, clap::Args)]
pub struct MockOptions {
    /// Nombre del modelo que anuncia (`model_alias` en `/props`).
    #[arg(long, default_value = "mock-model")]
    pub model: String,
    /// Número de slots (`--parallel` en llama-server).
    #[arg(long, default_value_t = 2)]
    pub slots: u32,
    #[arg(long, default_value_t = 4096)]
    pub ctx_size: u32,
    /// Tokens que genera cada respuesta.
    #[arg(long, default_value_t = 24)]
    pub tokens: u32,
    /// Retardo entre tokens, en milisegundos.
    #[arg(long, default_value_t = 40)]
    pub token_delay_ms: u64,
    /// Responder 500 a toda petición de inferencia (para probar reintentos).
    #[arg(long)]
    pub fail: bool,
    /// Texto que identifica a esta instancia en las respuestas.
    #[arg(long, default_value = "mock")]
    pub label: String,
}

impl Default for MockOptions {
    fn default() -> Self {
        Self {
            model: "mock-model".into(),
            slots: 2,
            ctx_size: 4096,
            tokens: 24,
            token_delay_ms: 40,
            fail: false,
            label: "mock".into(),
        }
    }
}

/// Estado compartido; las pruebas lo leen para saber cuántas peticiones llegaron.
#[derive(Debug, Default)]
pub struct MockState {
    pub opts: MockOptions,
    busy: AtomicU32,
    pub inference_requests: AtomicU64,
}

impl MockState {
    pub fn new(opts: MockOptions) -> Arc<Self> {
        Arc::new(Self {
            opts,
            ..Default::default()
        })
    }
}

pub fn router(state: Arc<MockState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/props", get(props))
        .route("/slots", get(slots))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn props(State(s): State<Arc<MockState>>) -> Json<Value> {
    Json(json!({
        "default_generation_settings": { "params": {}, "n_ctx": s.opts.ctx_size },
        "total_slots": s.opts.slots,
        "model_alias": s.opts.model,
        "model_path": format!("/models/{}.gguf", s.opts.model),
        "build_info": "b11963-mock",
        "is_sleeping": false,
    }))
}

async fn slots(State(s): State<Arc<MockState>>) -> Json<Value> {
    let busy = s.busy.load(Ordering::Relaxed);
    let list: Vec<Value> = (0..s.opts.slots)
        .map(|id| {
            json!({
                "id": id,
                "n_ctx": s.opts.ctx_size,
                "speculative": false,
                "is_processing": id < busy,
            })
        })
        .collect();
    Json(Value::Array(list))
}

async fn models(State(s): State<Arc<MockState>>) -> Json<Value> {
    Json(json!({
        "object": "list",
        "data": [{
            "id": s.opts.model,
            "object": "model",
            "created": 0,
            "owned_by": "llamacpp",
            "meta": { "n_ctx": s.opts.ctx_size },
        }],
    }))
}

#[derive(Debug, Default, Deserialize)]
struct InferenceRequest {
    #[serde(default)]
    stream: bool,
    max_tokens: Option<u32>,
}

#[derive(Clone, Copy)]
enum Kind {
    Chat,
    Text,
}

async fn chat_completions(State(s): State<Arc<MockState>>, body: Bytes) -> Response {
    infer(s, body, Kind::Chat).await
}

async fn completions(State(s): State<Arc<MockState>>, body: Bytes) -> Response {
    infer(s, body, Kind::Text).await
}

/// Marca un slot como ocupado mientras vive.
struct SlotGuard(Arc<MockState>);

impl SlotGuard {
    fn new(s: Arc<MockState>) -> Self {
        s.busy.fetch_add(1, Ordering::Relaxed);
        Self(s)
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.busy.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn infer(s: Arc<MockState>, body: Bytes, kind: Kind) -> Response {
    s.inference_requests.fetch_add(1, Ordering::Relaxed);
    if s.opts.fail {
        let err =
            json!({"error": {"code": 500, "message": "mock failure", "type": "server_error"}});
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(err)).into_response();
    }
    let req: InferenceRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            let err = json!({"error": {"code": 400, "message": e.to_string(), "type": "invalid_request_error"}});
            return (StatusCode::BAD_REQUEST, Json(err)).into_response();
        }
    };
    let n = req
        .max_tokens
        .unwrap_or(s.opts.tokens)
        .min(s.opts.tokens)
        .max(1);
    let delay = Duration::from_millis(s.opts.token_delay_ms);
    let guard = SlotGuard::new(s.clone());

    if req.stream {
        let stream = futures::stream::unfold(
            (0u32, Instant::now(), guard),
            move |(i, start, guard)| async move {
                if i > n {
                    return None;
                }
                if i == n {
                    let done = Bytes::from_static(b"data: [DONE]\n\n");
                    return Some((Ok::<_, Infallible>(done), (i + 1, start, guard)));
                }
                tokio::time::sleep(delay).await;
                let last = i + 1 == n;
                let chunk = stream_chunk(&guard.0.opts, kind, i, last, start, n);
                let bytes = Bytes::from(format!("data: {chunk}\n\n"));
                Some((Ok(bytes), (i + 1, start, guard)))
            },
        );
        return Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from_stream(stream))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }

    let start = Instant::now();
    tokio::time::sleep(delay * n).await;
    let text: String = (0..n).map(|i| word(&s.opts, i)).collect();
    let timings = timings(n, start);
    let usage = json!({"completion_tokens": n, "prompt_tokens": 8, "total_tokens": n + 8});
    let body = match kind {
        Kind::Chat => json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "created": 0,
            "model": s.opts.model,
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": { "role": "assistant", "content": text },
            }],
            "usage": usage,
            "timings": timings,
        }),
        Kind::Text => json!({
            "id": "cmpl-mock",
            "object": "text_completion",
            "created": 0,
            "model": s.opts.model,
            "choices": [{ "index": 0, "finish_reason": "stop", "text": text }],
            "usage": usage,
            "timings": timings,
        }),
    };
    drop(guard);
    Json(body).into_response()
}

fn word(opts: &MockOptions, i: u32) -> String {
    if i == 0 {
        format!("[{}]", opts.label)
    } else {
        format!(" token{i}")
    }
}

fn timings(n: u32, start: Instant) -> Value {
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    let per_second = if ms > 0.0 {
        f64::from(n) * 1000.0 / ms
    } else {
        0.0
    };
    json!({
        "cache_n": 0,
        "prompt_n": 8,
        "prompt_ms": 1.0,
        "prompt_per_token_ms": 0.125,
        "prompt_per_second": 8000.0,
        "predicted_n": n,
        "predicted_ms": ms,
        "predicted_per_token_ms": ms / f64::from(n.max(1)),
        "predicted_per_second": per_second,
    })
}

fn stream_chunk(
    opts: &MockOptions,
    kind: Kind,
    i: u32,
    last: bool,
    start: Instant,
    n: u32,
) -> Value {
    let text = word(opts, i);
    let finish = if last { json!("stop") } else { Value::Null };
    let mut chunk = match kind {
        Kind::Chat => json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": opts.model,
            "choices": [{ "index": 0, "finish_reason": finish, "delta": { "content": text } }],
        }),
        Kind::Text => json!({
            "id": "cmpl-mock",
            "object": "text_completion",
            "created": 0,
            "model": opts.model,
            "choices": [{ "index": 0, "finish_reason": finish, "text": text }],
        }),
    };
    if last {
        chunk["timings"] = timings(n, start);
    }
    chunk
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use tower::ServiceExt;

    fn fast() -> Arc<MockState> {
        MockState::new(MockOptions {
            tokens: 3,
            token_delay_ms: 1,
            ..Default::default()
        })
    }

    async fn call(
        state: Arc<MockState>,
        method: &str,
        uri: &str,
        body: &str,
    ) -> (StatusCode, String) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = router(state).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn props_and_slots_match_llama_server_shape() {
        let (_, props) = call(fast(), "GET", "/props", "").await;
        let props: Value = serde_json::from_str(&props).unwrap();
        assert_eq!(props["total_slots"], 2);
        assert_eq!(props["model_alias"], "mock-model");
        assert_eq!(props["default_generation_settings"]["n_ctx"], 4096);
        let (_, slots) = call(fast(), "GET", "/slots", "").await;
        let slots: Value = serde_json::from_str(&slots).unwrap();
        assert_eq!(slots.as_array().unwrap().len(), 2);
        assert_eq!(slots[0]["is_processing"], false);
    }

    #[tokio::test]
    async fn non_streaming_chat_has_usage_and_timings() {
        let s = fast();
        let (status, body) = call(
            s.clone(),
            "POST",
            "/v1/chat/completions",
            r#"{"messages":[]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["usage"]["completion_tokens"], 3);
        assert!(v["timings"]["predicted_per_second"].as_f64().unwrap() > 0.0);
        assert_eq!(s.inference_requests.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn streaming_emits_sse_chunks_and_done() {
        let (_, body) = call(fast(), "POST", "/v1/chat/completions", r#"{"stream":true}"#).await;
        let events: Vec<&str> = body.split("\n\n").filter(|e| !e.is_empty()).collect();
        assert_eq!(events.len(), 4);
        assert!(events[2].contains("\"timings\""));
        assert_eq!(events[3], "data: [DONE]");
    }

    #[tokio::test]
    async fn fail_mode_returns_500() {
        let s = MockState::new(MockOptions {
            fail: true,
            ..Default::default()
        });
        let (status, body) = call(s, "POST", "/v1/completions", "{}").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("server_error"));
    }
}
