//! Modo dividido (D-56): las capas del modelo repartidas entre teléfonos.
//!
//! Cada teléfono corre `rpc-server` (modo Worker) y el Mac corre la cabeza:
//! un `llama-server` sin aceleración local lanzado con `--rpc`, así que todas
//! las capas acaban en los teléfonos. La cabeza entra en el registro como un
//! nodo más («dividido») y el proxy le manda peticiones sin saber nada de esto.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use pc_proto::{Heartbeat, LlamaInfo, LlamaStatus, Mode, NodeId, NodeStatus, PROTO_VERSION};
use serde::Serialize;
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Child;
use tokio::sync::Notify;

use crate::config::Config;
use crate::events::Event;
use crate::registry::{NodeView, Registry};

/// Nombre de la cabeza en el registro y en `X-PC-Node`.
pub const HEAD_NAME: &str = "dividido";
const MIN_MEMBERS: usize = 2;
const STEP: Duration = Duration::from_secs(1);
const RETRY_AFTER: Duration = Duration::from_secs(15);
/// Lo que puede tardar un teléfono en parar llama-server y lanzar rpc-server.
const WORKERS_TIMEOUT: Duration = Duration::from_secs(90);
/// La cabeza envía el modelo entero por la Wi-Fi la primera vez.
const LOAD_TIMEOUT: Duration = Duration::from_secs(600);
const LOG_LINES: usize = 200;
/// Un teléfono que no arrancó rpc-server a tiempo queda fuera este rato,
/// para que no bloquee a los demás en cada reintento.
const BENCH_FOR: Duration = Duration::from_secs(600);

pub fn head_id() -> NodeId {
    NodeId::new_v5(&NodeId::NAMESPACE_OID, b"phone-cluster/split-head")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Off,
    /// Esperando a que los teléfonos pasen a rpc-server.
    Preparing,
    /// La cabeza está enviando las capas a los teléfonos.
    Loading,
    Ready,
    Error,
}

/// Capas de un teléfono: bloques `[primero, último]` y si lleva la capa de salida.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Layers {
    pub blocks: Option<(u32, u32)>,
    pub output: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Member {
    pub node_id: NodeId,
    pub name: String,
    /// `ip:puerto` de su rpc-server.
    pub endpoint: String,
    /// RAM libre al repartir: el peso de este teléfono en `--tensor-split`.
    pub share_mb: u64,
    pub layers: Option<Layers>,
    /// Lo que la cabeza dice que ocupa el modelo en este teléfono.
    pub model_mib: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SplitView {
    /// Id con el que la cabeza aparece entre los nodos.
    pub head_id: NodeId,
    pub enabled: bool,
    pub phase: Phase,
    pub message: Option<String>,
    pub model: Option<String>,
    pub n_layer: Option<u32>,
    pub members: Vec<Member>,
    /// Últimas líneas del log de la cabeza cuando falla.
    pub log_tail: Vec<String>,
}

impl SplitView {
    fn off() -> Self {
        Self {
            head_id: head_id(),
            enabled: false,
            phase: Phase::Off,
            message: None,
            model: None,
            n_layer: None,
            members: Vec::new(),
            log_tail: Vec::new(),
        }
    }
}

/// Reparto de llama.cpp b11963 (`src/llama-model.cpp`, `load_tensors`) con
/// `-ngl` ≥ capas: `n_layer` bloques más la capa de salida, repartidos según
/// las fracciones acumuladas de `shares`.
pub fn assign_layers(n_layer: u32, shares: &[f64]) -> Vec<Layers> {
    let n = shares.len();
    let mut out = vec![Layers::default(); n];
    if n == 0 {
        return out;
    }
    // f32 como llama.cpp, para caer en el mismo lado en los bordes.
    let total: f32 = shares.iter().map(|&s| s as f32).sum();
    let mut acc = 0.0f32;
    let splits: Vec<f32> = shares
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            acc += s as f32;
            if total != 0.0 {
                acc / total
            } else {
                (i + 1) as f32 / n as f32
            }
        })
        .collect();
    let act = (n_layer + 1) as f32;
    for il in 0..=n_layer {
        let x = il as f32 / act;
        let dev = splits.partition_point(|&c| c <= x).min(n - 1);
        let l = &mut out[dev];
        if il == n_layer {
            l.output = true;
        } else {
            l.blocks = Some(match l.blocks {
                Some((first, _)) => (first, il),
                None => (il, il),
            });
        }
    }
    out
}

/// `print_info: n_layer          = 28`
fn parse_n_layer(line: &str) -> Option<u32> {
    let rest = line.split("print_info: n_layer").nth(1)?;
    rest.trim_start().strip_prefix('=')?.trim().parse().ok()
}

/// `load_tensors: RPC0[192.168.1.5:50052] model buffer size =   408.14 MiB`
fn parse_model_buffer(line: &str) -> Option<(String, f64)> {
    let (head, tail) = line.split_once(" model buffer size =")?;
    let endpoint = head.rsplit_once('[')?.1.strip_suffix(']')?;
    let mib = tail.trim().strip_suffix("MiB")?.trim().parse().ok()?;
    Some((endpoint.to_string(), mib))
}

/// Lo que se va sabiendo del log de la cabeza.
#[derive(Debug, Default)]
struct HeadLog {
    lines: VecDeque<String>,
    n_layer: Option<u32>,
    model_mib: HashMap<String, f64>,
}

impl HeadLog {
    fn push(&mut self, line: String) {
        if let Some(n) = parse_n_layer(&line) {
            self.n_layer = Some(n);
        }
        if let Some((endpoint, mib)) = parse_model_buffer(&line) {
            self.model_mib.insert(endpoint, mib);
        }
        if self.lines.len() == LOG_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    fn tail(&self, n: usize) -> Vec<String> {
        let skip = self.lines.len().saturating_sub(n);
        self.lines.iter().skip(skip).cloned().collect()
    }
}

struct Head {
    child: Child,
    log: Arc<Mutex<HeadLog>>,
}

impl Drop for Head {
    fn drop(&mut self) {
        crate::terminate(&self.child);
    }
}

pub struct Split {
    registry: Arc<Registry>,
    cfg: Arc<Config>,
    client: reqwest::Client,
    state: Mutex<SplitView>,
    wake: Notify,
    benched: Mutex<HashMap<NodeId, Instant>>,
}

/// Por qué se interrumpió un ciclo.
enum Stop {
    Disabled,
    Failed(String),
}

impl Split {
    pub fn new(registry: Arc<Registry>, cfg: Arc<Config>, client: reqwest::Client) -> Arc<Self> {
        Arc::new(Self {
            registry,
            cfg,
            client,
            state: Mutex::new(SplitView::off()),
            wake: Notify::new(),
            benched: Mutex::default(),
        })
    }

    pub fn spawn(self: &Arc<Self>) {
        tokio::spawn(self.clone().run());
    }

    fn state(&self) -> MutexGuard<'_, SplitView> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn view(&self) -> SplitView {
        self.state().clone()
    }

    pub fn set_enabled(&self, enabled: bool) -> SplitView {
        let view = {
            let mut s = self.state();
            if s.enabled == enabled {
                return s.clone();
            }
            s.enabled = enabled;
            s.clone()
        };
        tracing::info!(enabled, "modo dividido");
        self.wake.notify_one();
        view
    }

    fn benched(&self) -> MutexGuard<'_, HashMap<NodeId, Instant>> {
        self.benched.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn enabled(&self) -> bool {
        self.state().enabled
    }

    fn publish(&self, f: impl FnOnce(&mut SplitView)) {
        let view = {
            let mut s = self.state();
            f(&mut s);
            s.clone()
        };
        self.registry
            .events()
            .publish(Event::SplitUpdated(Box::new(view)));
    }

    /// Espera `d` o hasta que alguien encienda o apague el modo.
    async fn sleep(&self, d: Duration) {
        tokio::select! {
            () = tokio::time::sleep(d) => {}
            () = self.wake.notified() => {}
        }
    }

    async fn run(self: Arc<Self>) {
        loop {
            if !self.enabled() {
                self.release();
                self.publish(|v| *v = SplitView::off());
                self.wake.notified().await;
                continue;
            }
            match self.cycle().await {
                Stop::Disabled => {}
                Stop::Failed(msg) => {
                    tracing::warn!(error = %msg, "modo dividido: fallo; se reintenta");
                    self.registry.remove(head_id());
                    self.publish(|v| {
                        v.phase = Phase::Error;
                        v.message = Some(msg);
                    });
                    self.sleep(RETRY_AFTER).await;
                }
            }
        }
    }

    /// Todos los teléfonos vuelven a modo solo y la cabeza sale del registro.
    fn release(&self) {
        let now = Instant::now();
        for n in self.registry.nodes(now) {
            if n.mode == Mode::Worker {
                self.registry.set_mode(n.node_id, Mode::Solo, now);
            }
        }
        self.registry.remove(head_id());
    }

    /// Prepara los teléfonos, lanza la cabeza y la vigila mientras funcione.
    async fn cycle(&self) -> Stop {
        let now = Instant::now();
        let nodes = self.registry.nodes(now);
        let chosen: Vec<&NodeView> = {
            let mut benched = self.benched();
            benched.retain(|_, since| now.duration_since(*since) < BENCH_FOR);
            nodes
                .iter()
                .filter(|n| eligible(n) && !benched.contains_key(&n.node_id))
                .collect()
        };
        if chosen.len() < MIN_MEMBERS {
            return Stop::Failed(format!(
                "Hacen falta al menos {MIN_MEMBERS} teléfonos con la app actualizada; ahora hay {}.",
                chosen.len()
            ));
        }
        for n in &nodes {
            let want = if chosen.iter().any(|c| c.node_id == n.node_id) {
                Mode::Worker
            } else {
                Mode::Solo
            };
            self.registry.set_mode(n.node_id, want, now);
        }
        let ids: Vec<NodeId> = chosen.iter().map(|n| n.node_id).collect();
        self.publish(|v| {
            v.phase = Phase::Preparing;
            v.message = Some("Los teléfonos están cambiando a rpc-server…".into());
            v.n_layer = None;
            v.log_tail.clear();
            v.members = chosen.iter().map(|n| member(n)).collect();
        });

        // 1. Esperar a que todos tengan rpc-server lanzado.
        let started = Instant::now();
        let mut left_out: Vec<String> = Vec::new();
        let members = loop {
            if !self.enabled() {
                return Stop::Disabled;
            }
            let nodes = self.registry.nodes(Instant::now());
            let current: Vec<&NodeView> = ids
                .iter()
                .filter_map(|id| nodes.iter().find(|n| n.node_id == *id))
                .collect();
            if current.len() == ids.len() && current.iter().all(|n| n.status == NodeStatus::Worker)
            {
                break current.into_iter().map(member).collect::<Vec<_>>();
            }
            if started.elapsed() > WORKERS_TIMEOUT {
                // Seguir sin los que no llegaron, si quedan bastantes.
                let (ready, late): (Vec<NodeId>, Vec<NodeId>) = ids.iter().partition(|id| {
                    current
                        .iter()
                        .any(|n| n.node_id == **id && n.status == NodeStatus::Worker)
                });
                let now = Instant::now();
                for id in &late {
                    self.registry.set_mode(*id, Mode::Solo, now);
                    self.benched().insert(*id, now);
                    left_out.push(name_of(&nodes, *id));
                }
                tracing::warn!(
                    ?left_out,
                    "sin rpc-server a tiempo: fuera del modo dividido 10 min"
                );
                if ready.len() < MIN_MEMBERS {
                    return Stop::Failed(format!(
                        "{} no arrancó rpc-server; vuelve al modo normal y se reintentará en 10 min.",
                        left_out.join(", ")
                    ));
                }
                break current
                    .into_iter()
                    .filter(|n| ready.contains(&n.node_id))
                    .map(member)
                    .collect::<Vec<_>>();
            }
            self.sleep(STEP).await;
        };

        // 2. Lanzar la cabeza.
        let Some(model) = crate::devices::find_model(&self.cfg.adb.artifacts_dir) else {
            return Stop::Failed(format!(
                "No hay ningún modelo .gguf en {}",
                self.cfg.adb.artifacts_dir.display()
            ));
        };
        let file = model
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Nombre propio: un cliente pide el modelo dividido eligiendo este
        // modelo, y `auto` sigue pudiendo ir a cualquier nodo.
        let alias = format!("{}-{HEAD_NAME}", self.cfg.adb.alias_for(&file));
        let bin = self.llama_bin();
        if !bin.is_file() {
            return Stop::Failed(format!(
                "Falta {}: ejecuta tools/build-android.sh",
                bin.display()
            ));
        }
        let args = head_args(
            &model.to_string_lossy(),
            &alias,
            &members,
            self.cfg.split.head_port,
            self.cfg.split.ctx_size,
        );
        tracing::info!(bin = %bin.display(), ?args, "lanzando la cabeza del modo dividido");
        let head = match spawn_head(&bin, &args) {
            Ok(h) => h,
            Err(e) => return Stop::Failed(format!("No se pudo lanzar la cabeza: {e}")),
        };
        let note = if left_out.is_empty() {
            String::new()
        } else {
            format!(
                " Sin {}: no arrancó rpc-server a tiempo.",
                left_out.join(", ")
            )
        };
        self.publish(|v| {
            v.phase = Phase::Loading;
            v.message = Some(format!("Enviando las capas a los teléfonos…{note}"));
            v.model = Some(alias.clone());
            v.members = members.clone();
        });

        // 3. Vigilar: cargando → listo, y que nadie se caiga.
        let mut head = head;
        let started = Instant::now();
        let mut ready = false;
        loop {
            if !self.enabled() {
                return Stop::Disabled;
            }
            if let Ok(Some(status)) = head.child.try_wait() {
                let tail = lock(&head.log).tail(12);
                self.publish(|v| v.log_tail = tail);
                return Stop::Failed(format!("La cabeza terminó ({status})."));
            }
            let nodes = self.registry.nodes(Instant::now());
            for m in &members {
                let status = nodes
                    .iter()
                    .find(|n| n.node_id == m.node_id)
                    .map(|n| n.status);
                if status != Some(NodeStatus::Worker) {
                    return Stop::Failed(format!(
                        "{} salió del modo dividido ({}).",
                        m.name,
                        status.map_or("desconectado".into(), |s| format!("{s:?}"))
                    ));
                }
            }
            if self.head_healthy().await {
                if !ready {
                    ready = true;
                    let (n_layer, mib) = {
                        let log = lock(&head.log);
                        (log.n_layer, log.model_mib.clone())
                    };
                    let shares: Vec<f64> = members.iter().map(|m| m.share_mb as f64).collect();
                    let layers = n_layer.map(|n| assign_layers(n, &shares));
                    tracing::info!(?n_layer, ?mib, "modo dividido listo");
                    let note = note.trim().to_string();
                    self.publish(|v| {
                        v.phase = Phase::Ready;
                        v.message = (!note.is_empty()).then_some(note);
                        v.n_layer = n_layer;
                        for (i, m) in v.members.iter_mut().enumerate() {
                            m.layers = layers.as_ref().and_then(|l| l.get(i).copied());
                            m.model_mib = mib.get(&m.endpoint).copied();
                        }
                    });
                }
                self.registry
                    .heartbeat(self.head_heartbeat(&alias), Instant::now());
            } else if ready {
                return Stop::Failed("La cabeza dejó de responder.".into());
            } else if started.elapsed() > LOAD_TIMEOUT {
                return Stop::Failed("La cabeza tardó demasiado en cargar el modelo.".into());
            }
            self.sleep(Duration::from_secs(2)).await;
        }
    }

    fn llama_bin(&self) -> PathBuf {
        self.cfg
            .split
            .llama_bin
            .clone()
            .unwrap_or_else(|| self.cfg.adb.artifacts_dir.join("macos/llama-server"))
    }

    fn head_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.cfg.split.head_port)
    }

    async fn head_healthy(&self) -> bool {
        self.client
            .get(format!("{}/health", self.head_url()))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }

    /// La cabeza se registra como un nodo más para que el proxy la use.
    fn head_heartbeat(&self, alias: &str) -> Heartbeat {
        let now = OffsetDateTime::now_utc();
        Heartbeat {
            proto_version: PROTO_VERSION,
            node_id: head_id(),
            name: HEAD_NAME.into(),
            agent_version: env!("CARGO_PKG_VERSION").into(),
            advertise_url: self.head_url(),
            sent_at: now.replace_nanosecond(0).unwrap_or(now),
            battery: None,
            thermal: None,
            memory: None,
            llama: Some(LlamaInfo {
                status: LlamaStatus::Ok,
                model: Some(alias.into()),
                ctx_size: Some(self.cfg.split.ctx_size),
                slots_total: Some(1),
                slots_busy: None,
            }),
            rpc: None,
            last_error: None,
            acked_commands: Vec::new(),
        }
    }
}

fn lock(log: &Mutex<HeadLog>) -> MutexGuard<'_, HeadLog> {
    log.lock().unwrap_or_else(|e| e.into_inner())
}

/// Puede prestar sus capas: tiene rpc-server y está sano (o ya es Worker).
fn eligible(n: &NodeView) -> bool {
    n.node_id != head_id()
        && n.rpc.is_some()
        && matches!(
            n.status,
            NodeStatus::Ready | NodeStatus::Busy | NodeStatus::Worker | NodeStatus::Down
        )
}

fn member(n: &NodeView) -> Member {
    let host = n
        .advertise_url
        .as_deref()
        .and_then(|u| reqwest::Url::parse(u).ok())
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_default();
    let port = n.rpc.as_ref().map_or(0, |r| r.port);
    Member {
        node_id: n.node_id,
        name: n.name.clone(),
        endpoint: format!("{host}:{port}"),
        share_mb: n.memory.as_ref().map_or(1, |m| m.available_mb.max(1)),
        layers: None,
        model_mib: None,
    }
}

fn name_of(nodes: &[NodeView], id: NodeId) -> String {
    nodes
        .iter()
        .find(|n| n.node_id == id)
        .map_or_else(|| id.to_string(), |n| n.name.clone())
}

fn head_args(model: &str, alias: &str, members: &[Member], port: u16, ctx: u32) -> Vec<String> {
    let rpc: Vec<&str> = members.iter().map(|m| m.endpoint.as_str()).collect();
    let split: Vec<String> = members.iter().map(|m| m.share_mb.to_string()).collect();
    [
        "-m",
        model,
        "--alias",
        alias,
        "--rpc",
        &rpc.join(","),
        // Más capas de las que hay: todas a los teléfonos.
        "-ngl",
        "999",
        "-ts",
        &split.join(","),
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "-c",
        &ctx.to_string(),
        "-np",
        "1",
        // Nivel 4: imprime n_layer y lo que ocupa el modelo en cada teléfono.
        "-lv",
        "4",
    ]
    .map(String::from)
    .to_vec()
}

fn spawn_head(bin: &std::path::Path, args: &[String]) -> std::io::Result<Head> {
    let mut child = crate::watched(&bin.to_string_lossy(), args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let log = Arc::new(Mutex::new(HeadLog::default()));
    if let Some(out) = child.stdout.take() {
        tokio::spawn(read_lines(out, log.clone()));
    }
    if let Some(err) = child.stderr.take() {
        tokio::spawn(read_lines(err, log.clone()));
    }
    Ok(Head { child, log })
}

async fn read_lines(src: impl AsyncRead + Unpin, log: Arc<Mutex<HeadLog>>) {
    let mut lines = BufReader::new(src).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!(target: "pc_coordinator::split::head", "{line}");
        lock(&log).push(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(l: &[Layers]) -> Vec<Option<(u32, u32)>> {
        l.iter().map(|x| x.blocks).collect()
    }

    #[test]
    fn layers_match_llama_cpp() {
        // Medido con b11963: 28 capas, -ts 1,1 → 15 bloques en el primero
        // (KV 30 MiB) y 13 más la salida en el segundo (KV 26 MiB).
        let l = assign_layers(28, &[1.0, 1.0]);
        assert_eq!(blocks(&l), vec![Some((0, 14)), Some((15, 27))]);
        assert_eq!((l[0].output, l[1].output), (false, true));
    }

    #[test]
    fn layers_follow_the_shares() {
        let l = assign_layers(28, &[3000.0, 1000.0]);
        let count = |b: Option<(u32, u32)>| b.map_or(0, |(a, z)| z - a + 1);
        assert!(count(l[0].blocks) > 2 * count(l[1].blocks));
        assert_eq!(count(l[0].blocks) + count(l[1].blocks), 28);
        // Sin pesos: a partes iguales.
        assert_eq!(
            blocks(&assign_layers(28, &[0.0, 0.0])),
            blocks(&assign_layers(28, &[5.0, 5.0]))
        );
        assert!(assign_layers(28, &[]).is_empty());
        let one = assign_layers(28, &[1.0]);
        assert_eq!(one[0].blocks, Some((0, 27)));
        assert!(one[0].output);
    }

    #[test]
    fn parses_head_log() {
        assert_eq!(
            parse_n_layer("0.00.189.257 I print_info: n_layer               = 28"),
            Some(28)
        );
        assert_eq!(parse_n_layer("print_info: n_layer_dense_lead = 3"), None);
        assert_eq!(
            parse_model_buffer(
                "0.00.232.789 I load_tensors: RPC0[192.168.1.5:50052] model buffer size =   408.14 MiB"
            ),
            Some(("192.168.1.5:50052".into(), 408.14))
        );
        assert_eq!(
            parse_model_buffer("load_tensors:   CPU_Mapped model buffer size =   125.19 MiB"),
            None
        );
        let mut log = HeadLog::default();
        for i in 0..(LOG_LINES + 5) {
            log.push(format!("l{i}"));
        }
        assert_eq!(log.lines.len(), LOG_LINES);
        assert_eq!(
            log.tail(2),
            vec![format!("l{}", LOG_LINES + 3), format!("l{}", LOG_LINES + 4)]
        );
    }

    #[test]
    fn head_sends_every_layer_to_the_phones() {
        let m = |ep: &str, mb| Member {
            node_id: NodeId::nil(),
            name: "n".into(),
            endpoint: ep.into(),
            share_mb: mb,
            layers: None,
            model_mib: None,
        };
        let args = head_args(
            "/m.gguf",
            "qwen",
            &[m("10.0.0.1:50052", 4000), m("10.0.0.2:50052", 2000)],
            7810,
            4096,
        );
        let joined = args.join(" ");
        assert!(joined.contains("--rpc 10.0.0.1:50052,10.0.0.2:50052"));
        assert!(joined.contains("-ts 4000,2000"));
        assert!(joined.contains("-ngl 999"));
        assert!(joined.contains("--host 127.0.0.1"));
    }
}
