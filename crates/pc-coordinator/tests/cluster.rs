//! Integración: coordinador, `mock-llama` y agentes `--mock` en puertos
//! efímeros dentro del propio test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use mock_llama::{MockOptions, MockState};
use pc_agent::config::{Config as AgentConfig, MockMetrics, Secret as AgentSecret};
use pc_coordinator::config::{AdbConfig, Config, Secret, SplitConfig};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const TOKEN: &str = "test-token";

struct Coordinator {
    api: String,
    dash: String,
    _task: JoinHandle<()>,
}

async fn coordinator(offline_after_secs: u64) -> Coordinator {
    coordinator_with(offline_after_secs, std::path::PathBuf::from("android/dist")).await
}

async fn coordinator_with(
    offline_after_secs: u64,
    artifacts_dir: std::path::PathBuf,
) -> Coordinator {
    let cfg = Config {
        token: Secret::new(TOKEN),
        offline_after_secs,
        advertise_mdns: false,
        adb: AdbConfig {
            enabled: false,
            artifacts_dir,
            ..AdbConfig::default()
        },
        ..Config::default()
    };
    start(cfg).await
}

async fn start(cfg: Config) -> Coordinator {
    let api = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dash = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (a, d) = (api.local_addr().unwrap(), dash.local_addr().unwrap());
    let task = tokio::spawn(async move {
        pc_coordinator::serve(cfg, api, dash).await.unwrap();
    });
    Coordinator {
        api: format!("http://{a}"),
        dash: format!("http://{d}"),
        _task: task,
    }
}

async fn mock(opts: MockOptions) -> (String, Arc<MockState>) {
    let state = MockState::new(opts);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = mock_llama::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, state)
}

fn agent(name: &str, coordinator: &str, llama_url: &str, temp_c: f32) -> JoinHandle<()> {
    let mut cfg = AgentConfig::new(name, coordinator);
    cfg.token = AgentSecret::new(TOKEN);
    cfg.llama_url = llama_url.into();
    cfg.heartbeat_secs = 1;
    cfg.mock = Some(MockMetrics {
        temp_c,
        battery_pct: 80,
        charging: true,
    });
    tokio::spawn(async move {
        pc_agent::run(cfg).await.unwrap();
    })
}

fn http() -> reqwest::Client {
    reqwest::Client::new()
}

async fn get_json(url: &str) -> Value {
    let bytes = http().get(url).send().await.unwrap().bytes().await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn nodes(c: &Coordinator) -> Vec<Value> {
    get_json(&format!("{}/api/v1/nodes", c.dash)).await["nodes"]
        .as_array()
        .unwrap()
        .clone()
}

/// Espera hasta `secs` a que `f` sea cierto; devuelve los segundos que tardó.
async fn wait_until<F, Fut>(secs: u64, mut f: F) -> f64
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        if f().await {
            return start.elapsed().as_secs_f64();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("la condición no se cumplió en {secs} s");
}

async fn ready_count(c: &Coordinator) -> usize {
    nodes(c)
        .await
        .iter()
        .filter(|n| n["status"] == "Ready")
        .count()
}

fn fast_mock(label: &str) -> MockOptions {
    MockOptions {
        tokens: 5,
        token_delay_ms: 20,
        label: label.into(),
        ..MockOptions::default()
    }
}

async fn chat(c: &Coordinator, body: Value) -> reqwest::Response {
    http()
        .post(format!("{}/v1/chat/completions", c.api))
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_appears_and_goes_offline() {
    let c = coordinator(3).await;
    let (url, _m) = mock(fast_mock("a")).await;
    let a = agent("phone-a", &c.api, &url, 36.0);

    let took = wait_until(10, || async { ready_count(&c).await == 1 }).await;
    assert!(took < 10.0, "apareció en {took:.1} s");
    let n = &nodes(&c).await[0];
    assert_eq!(n["name"], "phone-a");
    assert_eq!(n["llama"]["model"], "mock-model");
    assert_eq!(n["free_slots"], 2);

    a.abort();
    let took = wait_until(10, || async { nodes(&c).await[0]["status"] == "Offline" }).await;
    assert!(took >= 2.0, "Offline demasiado pronto: {took:.1} s");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thirty_requests_spread_over_three_nodes() {
    let c = coordinator(15).await;
    let mut mocks = vec![];
    let mut agents = vec![];
    for i in 0..3 {
        let (url, state) = mock(fast_mock(&format!("m{i}"))).await;
        agents.push(agent(&format!("phone-{i}"), &c.api, &url, 36.0));
        mocks.push(state);
    }
    wait_until(10, || async { ready_count(&c).await == 3 }).await;

    let mut set = tokio::task::JoinSet::new();
    for i in 0..30 {
        let body = json!({"model": "auto", "messages": [{"role": "user", "content": format!("hola {i}")}]});
        let api = c.api.clone();
        set.spawn(async move {
            let res = http()
                .post(format!("{api}/v1/chat/completions"))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 200);
            let v: Value = serde_json::from_slice(&res.bytes().await.unwrap()).unwrap();
            assert_eq!(v["object"], "chat.completion");
        });
    }
    while let Some(r) = set.join_next().await {
        r.unwrap();
    }
    let counts: Vec<u64> = mocks
        .iter()
        .map(|m| m.inference_requests.load(Ordering::Relaxed))
        .collect();
    assert_eq!(counts.iter().sum::<u64>(), 30);
    assert!(
        counts.iter().all(|&n| n > 0),
        "reparto desigual: {counts:?}"
    );

    // El coordinador contabiliza las peticiones atendidas por nodo.
    wait_until(5, || async {
        let served: u64 = nodes(&c)
            .await
            .iter()
            .map(|n| n["requests_served"].as_u64().unwrap())
            .sum();
        served == 30
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_nodes_is_503_with_openai_error() {
    let c = coordinator(15).await;
    let res = chat(&c, json!({"model": "auto", "messages": []})).await;
    assert_eq!(res.status(), 503);
    let v: Value = serde_json::from_slice(&res.bytes().await.unwrap()).unwrap();
    assert_eq!(v["error"]["type"], "no_nodes_available");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_model_is_503_and_models_lists_union() {
    let c = coordinator(15).await;
    let (url, _m) = mock(fast_mock("a")).await;
    let _a = agent("phone-a", &c.api, &url, 36.0);
    wait_until(10, || async { ready_count(&c).await == 1 }).await;

    let models = get_json(&format!("{}/v1/models", c.api)).await;
    assert_eq!(models["data"][0]["id"], "mock-model");
    let res = chat(&c, json!({"model": "otro", "messages": []})).await;
    assert_eq!(res.status(), 503);
    let res = chat(&c, json!({"model": "mock-model", "messages": []})).await;
    assert_eq!(res.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_and_incompatible_heartbeats() {
    let c = coordinator(15).await;
    let hb = |v: u32| {
        json!({
            "proto_version": v,
            "node_id": "8f0c2a4e-1b7d-4c3e-9a51-0d6f2b8e7c10",
            "name": "futuro",
            "agent_version": "9.0.0",
            "advertise_url": "http://10.0.0.9:8080",
            "sent_at": "2026-10-28T15:04:05Z"
        })
    };
    let url = format!("{}/api/v1/heartbeat", c.api);
    let res = http().post(&url).json(&hb(1)).send().await.unwrap();
    assert_eq!(res.status(), 401);
    let res = http()
        .post(&url)
        .bearer_auth("wrong")
        .json(&hb(1))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);

    let res = http()
        .post(&url)
        .bearer_auth(TOKEN)
        .json(&hb(2))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 422);
    let n = &nodes(&c).await[0];
    assert_eq!(n["status"], "Incompatible");
    assert_eq!(n["name"], "futuro");

    let res = http()
        .post(&url)
        .bearer_auth(TOKEN)
        .json(&json!({"proto_version": 1, "name": "roto"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 422);

    let res = http()
        .post(&url)
        .bearer_auth(TOKEN)
        .json(&hb(1))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let v: Value = serde_json::from_slice(&res.bytes().await.unwrap()).unwrap();
    assert_eq!(v, json!({"commands": [], "mode": "solo"}));

    // Administración: token en el listener de la API, libre en el del dashboard.
    let admin = format!("{}/api/v1/nodes", c.api);
    assert_eq!(http().get(&admin).send().await.unwrap().status(), 401);
    assert_eq!(
        http()
            .get(&admin)
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        get_json(&format!("{}/healthz", c.api)).await["status"],
        "ok"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_and_forget() {
    let c = coordinator(2).await;
    let (url, m) = mock(fast_mock("a")).await;
    let a = agent("phone-a", &c.api, &url, 36.0);
    wait_until(10, || async { ready_count(&c).await == 1 }).await;
    let id = nodes(&c).await[0]["node_id"].as_str().unwrap().to_string();

    let drain = |on: bool| {
        http()
            .post(format!("{}/api/v1/nodes/{id}/drain", c.dash))
            .json(&json!({ "drain": on }))
            .send()
    };
    assert_eq!(drain(true).await.unwrap().status(), 200);
    assert_eq!(nodes(&c).await[0]["status"], "Draining");
    let res = chat(&c, json!({"messages": []})).await;
    assert_eq!(res.status(), 503, "un nodo Draining no recibe peticiones");
    assert_eq!(m.inference_requests.load(Ordering::Relaxed), 0);
    drain(false).await.unwrap();
    assert_eq!(nodes(&c).await[0]["status"], "Ready");

    let forget = || {
        http()
            .delete(format!("{}/api/v1/nodes/{id}", c.dash))
            .send()
    };
    assert_eq!(forget().await.unwrap().status(), 409);
    a.abort();
    wait_until(10, || async { nodes(&c).await[0]["status"] == "Offline" }).await;
    assert_eq!(forget().await.unwrap().status(), 204);
    assert!(nodes(&c).await.is_empty());
    assert_eq!(forget().await.unwrap().status(), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_passes_chunks_through_without_buffering() {
    let c = coordinator(15).await;
    let (url, _m) = mock(MockOptions {
        tokens: 10,
        token_delay_ms: 100,
        label: "lento".into(),
        ..MockOptions::default()
    })
    .await;
    let _a = agent("phone-a", &c.api, &url, 36.0);
    wait_until(10, || async { ready_count(&c).await == 1 }).await;

    let start = std::time::Instant::now();
    let mut res = chat(&c, json!({"stream": true, "messages": []})).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-pc-node"], "phone-a");
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    let mut chunks = vec![];
    let mut first_at = None;
    while let Some(chunk) = res.chunk().await.unwrap() {
        first_at.get_or_insert(start.elapsed());
        chunks.push(String::from_utf8(chunk.to_vec()).unwrap());
    }
    let total = start.elapsed();
    let first = first_at.unwrap();
    assert!(
        first < Duration::from_millis(500) && total >= Duration::from_millis(900),
        "primer chunk a {first:?}, total {total:?}"
    );
    assert!(chunks.len() >= 5, "llegaron {} chunks", chunks.len());
    let text = chunks.concat();
    assert!(text.contains("[lento]"));
    assert!(text.trim_end().ends_with("data: [DONE]"));

    // El coordinador leyó tok/s del último chunk sin alterar el stream.
    wait_until(5, || async {
        nodes(&c).await[0]["last_tok_s"].as_f64().is_some()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retries_on_another_node_after_500() {
    let c = coordinator(15).await;
    let (bad_url, bad) = mock(MockOptions {
        fail: true,
        ..fast_mock("bad")
    })
    .await;
    let (good_url, good) = mock(fast_mock("good")).await;
    // El nodo que falla está más fresco, así que la política lo elige primero.
    let _a = agent("bad", &c.api, &bad_url, 30.0);
    let _b = agent("good", &c.api, &good_url, 36.0);
    wait_until(10, || async { ready_count(&c).await == 2 }).await;

    for stream in [false, true] {
        for _ in 0..5 {
            let res = chat(&c, json!({"stream": stream, "messages": []})).await;
            assert_eq!(res.status(), 200);
            assert_eq!(res.headers()["x-pc-node"], "good");
            let text = String::from_utf8(res.bytes().await.unwrap().to_vec()).unwrap();
            assert!(text.contains("[good]"));
        }
    }
    assert_eq!(bad.inference_requests.load(Ordering::Relaxed), 10);
    assert_eq!(good.inference_requests.load(Ordering::Relaxed), 10);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exhausted_retries_return_last_5xx_and_dead_node_is_502() {
    let c = coordinator(15).await;
    let (bad_url, bad) = mock(MockOptions {
        fail: true,
        ..fast_mock("bad")
    })
    .await;
    let _a = agent("bad", &c.api, &bad_url, 30.0);
    wait_until(10, || async { ready_count(&c).await == 1 }).await;
    let res = chat(&c, json!({"messages": []})).await;
    assert_eq!(res.status(), 500);
    assert_eq!(res.headers()["x-pc-node"], "bad");
    assert_eq!(
        bad.inference_requests.load(Ordering::Relaxed),
        1,
        "no reintenta en el mismo nodo"
    );

    // Un nodo que anuncia una URL donde no escucha nadie: 502.
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_url = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    let c2 = coordinator(15).await;
    let (url, _m) = mock(fast_mock("x")).await;
    let mut cfg = AgentConfig::new("ghost", &c2.api);
    cfg.token = AgentSecret::new(TOKEN);
    cfg.llama_url = url;
    cfg.advertise_url = Some(dead_url);
    cfg.heartbeat_secs = 1;
    cfg.mock = Some(MockMetrics {
        temp_c: 30.0,
        battery_pct: 80,
        charging: true,
    });
    let _g = tokio::spawn(async move { pc_agent::run(cfg).await.unwrap() });
    wait_until(10, || async { ready_count(&c2).await == 1 }).await;
    let res = chat(&c2, json!({"messages": []})).await;
    assert_eq!(res.status(), 502);
    let v: Value = serde_json::from_slice(&res.bytes().await.unwrap()).unwrap();
    assert_eq!(v["error"]["type"], "upstream_unreachable");
    assert_eq!(nodes(&c2).await[0]["in_flight"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hot_hysteresis_with_mock_temperatures() {
    let c = coordinator(15).await;
    let (url, _m) = mock(fast_mock("a")).await;
    let status = || async { nodes(&c).await.first().map(|n| n["status"].clone()) };
    let hot = agent("phone-a", &c.api, &url, 46.0);
    wait_until(10, || async { status().await == Some(json!("Hot")) }).await;
    assert_eq!(
        chat(&c, json!({"messages": []})).await.status(),
        503,
        "Hot sale de rotación"
    );
    hot.abort();

    // 42 °C está entre resume (40) y max (45): sigue Hot.
    let warm = agent("phone-a", &c.api, &url, 42.0);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(status().await, Some(json!("Hot")));
    warm.abort();

    let _cool = agent("phone-a", &c.api, &url, 39.0);
    wait_until(10, || async { status().await == Some(json!("Ready")) }).await;
    assert_eq!(chat(&c, json!({"messages": []})).await.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_llama_is_delivered_and_acked() {
    let c = coordinator(15).await;
    let (url, _m) = mock(fast_mock("a")).await;
    let _a = agent("phone-a", &c.api, &url, 36.0);
    wait_until(10, || async { ready_count(&c).await == 1 }).await;
    let id = nodes(&c).await[0]["node_id"].as_str().unwrap().to_string();

    let res = http()
        .post(format!("{}/api/v1/nodes/{id}/restart_llama", c.dash))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 202);
    assert_eq!(
        nodes(&c).await[0]["pending_commands"],
        json!(["restart_llama"])
    );
    // El agente (sin llama_command) lo recibe, lo registra y lo confirma.
    wait_until(10, || async {
        nodes(&c).await[0]["pending_commands"] == json!([])
    })
    .await;

    let res = http()
        .post(format!(
            "{}/api/v1/nodes/{}/restart_llama",
            c.dash,
            uuid::Uuid::nil()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn android_app_join_flow_model_apk_and_qr() {
    let dir = std::env::temp_dir().join(format!("pc-join-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let c = coordinator_with(15, dir.clone()).await;
    let join = |body: Value| {
        http()
            .post(format!("{}/api/v1/join", c.api))
            .json(&body)
            .send()
    };
    let req = json!({"device_id": "dev-1", "model": "SM-A546B", "label": "Galaxy A54"});

    // Sin APK todavía: 404 con un mensaje claro; el QR siempre existe.
    assert_eq!(
        http()
            .get(format!("{}/app", c.api))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let qr = http()
        .get(format!("{}/api/v1/app-qr.svg", c.dash))
        .send()
        .await
        .unwrap();
    assert_eq!(qr.headers()["content-type"], "image/svg+xml");
    assert!(qr.text().await.unwrap().contains("<svg"));

    // Pendiente hasta que se acepte en el dashboard.
    let v: Value = join(req.clone()).await.unwrap().json().await.unwrap();
    assert_eq!(v, json!({"status": "pending"}));
    let list = get_json(&format!("{}/api/v1/joins", c.dash)).await;
    assert_eq!(list["requests"][0]["node_name"], "galaxy-a54-1");
    assert_eq!(list["app_ready"], false);
    assert!(list["app_url"].as_str().unwrap().ends_with("/join"));
    let page = http().get(format!("{}/join", c.api)).send().await.unwrap();
    assert!(page.text().await.unwrap().contains("phonecluster"));
    let res = http()
        .post(format!("{}/api/v1/joins/dev-1/approve", c.dash))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    // Aprobado: recibe el token y lo que necesita, aunque aún no haya modelo.
    let v: Value = join(req.clone()).await.unwrap().json().await.unwrap();
    assert_eq!(v["status"], "approved");
    assert_eq!(v["config"]["token"], TOKEN);
    assert_eq!(v["config"]["name"], "galaxy-a54-1");
    assert_eq!(v["config"]["model_file"], Value::Null);
    let model_url = format!("{}/api/v1/files/model", c.api);
    assert_eq!(
        http()
            .get(&model_url)
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .status(),
        503
    );

    // Con modelo y APK en la carpeta de artefactos.
    std::fs::write(dir.join("tiny.gguf"), b"GGUF0123456789").unwrap();
    std::fs::write(dir.join("phone-cluster.apk"), b"PK-fake-apk").unwrap();
    let v: Value = join(req.clone()).await.unwrap().json().await.unwrap();
    assert_eq!(v["config"]["model_file"], "tiny.gguf");
    assert_eq!(v["config"]["model_size"], 14);
    assert_eq!(v["config"]["model_alias"], "tiny");
    assert_eq!(http().get(&model_url).send().await.unwrap().status(), 401);
    let res = http()
        .get(&model_url)
        .bearer_auth(TOKEN)
        .header("range", "bytes=4-")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 206, "admite reanudar descargas");
    assert_eq!(res.bytes().await.unwrap().as_ref(), b"0123456789");
    let apk = http().get(format!("{}/app", c.api)).send().await.unwrap();
    assert_eq!(apk.status(), 200);
    assert_eq!(
        apk.headers()["content-type"],
        "application/vnd.android.package-archive"
    );

    // Otro teléfono con el token vigente entra sin aprobación (reinicio del Mac).
    let v: Value = join(json!({"device_id": "dev-2", "model": "Pixel 7", "token": TOKEN}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["status"], "approved");
    // El dashboard no acepta peticiones de otras webs (CSRF) ni otros Host (DNS rebinding).
    let evil = http()
        .post(format!("{}/api/v1/joins/dev-1/approve", c.dash))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(evil.status(), 403);
    let rebound = http()
        .get(format!("{}/api/v1/nodes", c.dash))
        .header("host", "evil.example:7801")
        .send()
        .await
        .unwrap();
    assert_eq!(rebound.status(), 403);

    // Rechazar y datos inválidos.
    http()
        .post(format!("{}/api/v1/joins/dev-1/reject", c.dash))
        .send()
        .await
        .unwrap();
    let v: Value = join(req).await.unwrap().json().await.unwrap();
    assert_eq!(v, json!({"status": "rejected"}));
    let res = join(json!({"device_id": "bad id", "model": "x"}))
        .await
        .unwrap();
    assert_eq!(res.status(), 422);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Modo dividido de punta a punta hasta lanzar la cabeza: los agentes pasan a
/// rpc-server, el coordinador los reparte y, al apagarlo, vuelven a modo solo.
#[tokio::test]
async fn split_mode_switches_phones_to_workers_and_back() {
    let dir = std::env::temp_dir().join(format!("pc-split-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("m.gguf"), b"gguf").unwrap();
    let cfg = Config {
        token: Secret::new(TOKEN),
        advertise_mdns: false,
        adb: AdbConfig {
            enabled: false,
            artifacts_dir: dir.clone(),
            ..AdbConfig::default()
        },
        split: SplitConfig {
            llama_bin: Some(dir.join("no-existe")),
            ..SplitConfig::default()
        },
        ..Config::default()
    };
    let c = start(cfg).await;
    let split = |action: &str| {
        let url = format!("{}/api/v1/split/{action}", c.dash);
        async move { http().post(url).send().await.unwrap() }
    };
    let view = || get_json_owned(format!("{}/api/v1/split", c.dash));

    // Solo el dashboard local puede encenderlo.
    let res = http()
        .post(format!("{}/api/v1/split/on", c.api))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    let res = http()
        .post(format!("{}/api/v1/split/on", c.dash))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    assert_eq!(view().await["phase"], "off");

    // Un teléfono no basta.
    let (llama, _) = mock(MockOptions::default()).await;
    let _a1 = rpc_agent("p1", &c.api, &llama, &dir);
    wait_until(10, || async { nodes(&c).await.len() == 1 }).await;
    assert_eq!(split("on").await.status(), 200);
    wait_until(10, || async { view().await["phase"] == "error" }).await;
    assert!(
        view().await["message"]
            .as_str()
            .unwrap()
            .contains("al menos 2")
    );

    // Con dos, pasan a Worker y el fallo es ya la cabeza (no hay binario).
    let _a2 = rpc_agent("p2", &c.api, &llama, &dir);
    split("off").await;
    wait_until(10, || async { view().await["phase"] == "off" }).await;
    wait_until(10, || async { nodes(&c).await.len() == 2 }).await;
    split("on").await;
    wait_until(15, || async {
        nodes(&c).await.iter().all(|n| n["status"] == "Worker")
    })
    .await;
    wait_until(10, || async { view().await["phase"] == "error" }).await;
    let v = view().await;
    assert!(v["message"].as_str().unwrap().contains("no-existe"), "{v}");
    let members = v["members"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    assert!(
        members[0]["endpoint"]
            .as_str()
            .unwrap()
            .starts_with("127.0.0.1:")
    );

    // Al apagarlo vuelven a llama-server.
    split("off").await;
    wait_until(10, || async {
        nodes(&c)
            .await
            .iter()
            .all(|n| n["status"] == "Ready" && n["mode"] == "solo")
    })
    .await;
    let _ = std::fs::remove_dir_all(dir);
}

async fn get_json_owned(url: String) -> Value {
    get_json(&url).await
}

/// Agente con un rpc-server de mentira: `sh -c` ignora los `-H`/`-p` que añade.
fn rpc_agent(
    name: &str,
    coordinator: &str,
    llama_url: &str,
    dir: &std::path::Path,
) -> JoinHandle<()> {
    let mut cfg = AgentConfig::new(name, coordinator);
    cfg.token = AgentSecret::new(TOKEN);
    cfg.llama_url = llama_url.into();
    cfg.heartbeat_secs = 1;
    cfg.state_dir = dir.join(name);
    cfg.rpc_command = ["sh", "-c", "exec sleep 60"].map(String::from).to_vec();
    cfg.mock = Some(MockMetrics {
        temp_c: 30.0,
        battery_pct: 80,
        charging: true,
    });
    tokio::spawn(async move {
        pc_agent::run(cfg).await.unwrap();
    })
}
