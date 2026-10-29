//! Registro en memoria de los nodos y cálculo de su estado.
//!
//! `compute_status`, `hot_latch` y `free_slots` son funciones puras: reciben
//! `now` y no tocan el reloj ni hacen I/O.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use pc_proto::{
    Battery, Command, CommandKind, Heartbeat, HeartbeatHeader, HeartbeatResponse, LlamaInfo,
    LlamaStatus, Memory, Mode, NodeId, NodeStatus, RpcInfo, Thermal,
};
use serde::Serialize;

use crate::events::{Event, EventBus, RequestDone};
use crate::policy::Candidate;

#[derive(Debug, Clone)]
pub struct Thresholds {
    pub offline_after: Duration,
    /// Batería: lo que importa para la seguridad (D-52).
    pub max_temp_c: f32,
    pub resume_temp_c: f32,
    /// Procesador: bajo carga es normal que pase de 45 °C.
    pub max_soc_temp_c: f32,
    pub resume_soc_temp_c: f32,
    pub min_battery_pct: u8,
}

#[derive(Debug, Clone)]
pub struct NodeEntry {
    pub id: NodeId,
    pub name: String,
    /// `None` si el nodo es incompatible.
    pub heartbeat: Option<Heartbeat>,
    pub incompatible: Option<String>,
    pub last_seen: Instant,
    pub draining: bool,
    /// Memoria de la histéresis de Hot.
    pub hot: bool,
    /// Peticiones que el coordinador ha reenviado y aún no han terminado.
    pub in_flight: u32,
    pub status: NodeStatus,
    pub requests_served: u64,
    pub last_tok_s: Option<f64>,
    /// Comandos enviados al agente y aún no confirmados en `acked_commands`.
    pub pending: Vec<Command>,
    /// Lo que el coordinador quiere que ejecute el teléfono (modo dividido).
    pub mode: Mode,
}

impl NodeEntry {
    fn new(id: NodeId, name: String, now: Instant) -> Self {
        Self {
            id,
            name,
            heartbeat: None,
            incompatible: None,
            last_seen: now,
            draining: false,
            hot: false,
            in_flight: 0,
            status: NodeStatus::Offline,
            requests_served: 0,
            last_tok_s: None,
            pending: Vec::new(),
            mode: Mode::Solo,
        }
    }
}

/// Slots libres: `slots_total − máx(slots_busy, in_flight)`.
///
/// Sin datos de slots se asume 1 slot y 0 ocupados (ver DECISIONS.md, D-11).
pub fn free_slots(llama: Option<&LlamaInfo>, in_flight: u32) -> u32 {
    let total = llama.and_then(|l| l.slots_total).unwrap_or(1);
    let busy = llama.and_then(|l| l.slots_busy).unwrap_or(0);
    total.saturating_sub(busy.max(in_flight))
}

/// Temperaturas de batería y SoC de un heartbeat.
pub fn temps(hb: Option<&Heartbeat>) -> (Option<f32>, Option<f32>) {
    let battery = hb.and_then(|h| h.battery.as_ref()?.temp_c);
    let soc = hb.and_then(|h| h.thermal.as_ref()).map(|t| t.max_c);
    (battery, soc)
}

/// Histéresis con dos sensores: entra en Hot si la batería llega a
/// `max_temp_c` o el SoC a `max_soc_temp_c`; sale cuando los dos bajan de
/// su `resume_*`. Sin ninguna lectura se conserva el valor anterior.
pub fn hot_latch(battery: Option<f32>, soc: Option<f32>, prev: bool, t: &Thresholds) -> bool {
    let over =
        battery.is_some_and(|c| c >= t.max_temp_c) || soc.is_some_and(|c| c >= t.max_soc_temp_c);
    if over {
        return true;
    }
    if battery.is_none() && soc.is_none() {
        return prev;
    }
    let cool =
        battery.is_none_or(|c| c < t.resume_temp_c) && soc.is_none_or(|c| c < t.resume_soc_temp_c);
    if cool { false } else { prev }
}

/// Estado del nodo; gana la primera regla que aplique, en el orden de la
/// especificación. `hot` es el resultado de [`hot_latch`].
pub fn compute_status(e: &NodeEntry, hot: bool, now: Instant, t: &Thresholds) -> NodeStatus {
    if now.saturating_duration_since(e.last_seen) > t.offline_after {
        return NodeStatus::Offline;
    }
    if e.incompatible.is_some() {
        return NodeStatus::Incompatible;
    }
    let Some(hb) = &e.heartbeat else {
        return NodeStatus::Offline;
    };
    if e.draining {
        return NodeStatus::Draining;
    }
    if e.mode == Mode::Worker {
        // Sin llama-server propio: Down hasta que su rpc-server esté lanzado.
        return if hb.rpc.as_ref().is_some_and(|r| r.running) {
            NodeStatus::Worker
        } else {
            NodeStatus::Down
        };
    }
    let llama = hb.llama.as_ref();
    if llama.map(|l| l.status) != Some(LlamaStatus::Ok) {
        return NodeStatus::Down;
    }
    if hot {
        return NodeStatus::Hot;
    }
    if let Some(b) = &hb.battery
        && b.pct < t.min_battery_pct
        && !b.charging
    {
        return NodeStatus::LowBattery;
    }
    if free_slots(llama, e.in_flight) == 0 {
        return NodeStatus::Busy;
    }
    NodeStatus::Ready
}

/// Lo que ve el dashboard de cada nodo.
#[derive(Debug, Clone, Serialize)]
pub struct NodeView {
    pub node_id: NodeId,
    pub name: String,
    pub status: NodeStatus,
    pub incompatible_reason: Option<String>,
    pub last_seen_secs: f64,
    pub draining: bool,
    pub in_flight: u32,
    pub free_slots: u32,
    pub requests_served: u64,
    pub last_tok_s: Option<f64>,
    pub pending_commands: Vec<CommandKind>,
    pub agent_version: Option<String>,
    pub advertise_url: Option<String>,
    pub battery: Option<Battery>,
    pub thermal: Option<Thermal>,
    pub memory: Option<Memory>,
    pub llama: Option<LlamaInfo>,
    pub rpc: Option<RpcInfo>,
    pub mode: Mode,
    pub last_error: Option<String>,
}

impl NodeView {
    fn of(e: &NodeEntry, now: Instant) -> Self {
        let hb = e.heartbeat.as_ref();
        Self {
            node_id: e.id,
            name: e.name.clone(),
            status: e.status,
            incompatible_reason: e.incompatible.clone(),
            last_seen_secs: now.saturating_duration_since(e.last_seen).as_secs_f64(),
            draining: e.draining,
            in_flight: e.in_flight,
            free_slots: free_slots(hb.and_then(|h| h.llama.as_ref()), e.in_flight),
            requests_served: e.requests_served,
            last_tok_s: e.last_tok_s,
            pending_commands: e.pending.iter().map(|c| c.kind).collect(),
            agent_version: hb.map(|h| h.agent_version.clone()),
            advertise_url: hb.map(|h| h.advertise_url.clone()),
            battery: hb.and_then(|h| h.battery.clone()),
            thermal: hb.and_then(|h| h.thermal.clone()),
            memory: hb.and_then(|h| h.memory.clone()),
            llama: hb.and_then(|h| h.llama.clone()),
            rpc: hb.and_then(|h| h.rpc.clone()),
            mode: e.mode,
            last_error: hb.and_then(|h| h.last_error.clone()),
        }
    }
}

/// Destino de una petición elegida por la política.
#[derive(Debug, Clone)]
pub struct Target {
    pub id: NodeId,
    pub name: String,
    pub url: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ForgetError {
    NotFound,
    NotOffline,
}

pub struct Registry {
    nodes: RwLock<HashMap<NodeId, NodeEntry>>,
    thresholds: Thresholds,
    events: EventBus,
    /// Fin de las peticiones del último minuto, en ms Unix.
    recent: Mutex<VecDeque<i64>>,
}

impl Registry {
    pub fn new(thresholds: Thresholds) -> Self {
        Self {
            nodes: RwLock::default(),
            thresholds,
            events: EventBus::default(),
            recent: Mutex::default(),
        }
    }

    pub fn events(&self) -> &EventBus {
        &self.events
    }

    // Un pánico con el lock tomado no debe tumbar el coordinador entero.
    fn read(&self) -> RwLockReadGuard<'_, HashMap<NodeId, NodeEntry>> {
        self.nodes.read().unwrap_or_else(|e| e.into_inner())
    }
    fn write(&self) -> RwLockWriteGuard<'_, HashMap<NodeId, NodeEntry>> {
        self.nodes.write().unwrap_or_else(|e| e.into_inner())
    }
    fn recent(&self) -> MutexGuard<'_, VecDeque<i64>> {
        self.recent.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Recalcula Hot y el estado; devuelve si el estado cambió.
    fn refresh(&self, e: &mut NodeEntry, now: Instant) -> bool {
        let (battery, soc) = temps(e.heartbeat.as_ref());
        e.hot = hot_latch(battery, soc, e.hot, &self.thresholds);
        let status = compute_status(e, e.hot, now, &self.thresholds);
        let changed = status != e.status;
        e.status = status;
        changed
    }

    fn emit(&self, e: &NodeEntry, now: Instant) {
        self.events
            .publish(Event::NodeUpdated(Box::new(NodeView::of(e, now))));
    }

    /// Upsert por `node_id`. Quita los comandos confirmados y devuelve los
    /// pendientes (se reenvían hasta que el agente los confirme) y el modo.
    pub fn heartbeat(&self, hb: Heartbeat, now: Instant) -> HeartbeatResponse {
        let mut nodes = self.write();
        let e = nodes
            .entry(hb.node_id)
            .or_insert_with(|| NodeEntry::new(hb.node_id, hb.name.clone(), now));
        e.name = hb.name.clone();
        e.pending.retain(|c| !hb.acked_commands.contains(&c.id));
        for id in &hb.acked_commands {
            tracing::info!(node = %e.name, command = %id, "comando confirmado");
        }
        e.heartbeat = Some(hb);
        e.incompatible = None;
        e.last_seen = now;
        self.refresh(e, now);
        self.emit(e, now);
        HeartbeatResponse {
            commands: e.pending.clone(),
            mode: e.mode,
        }
    }

    /// Cambia el modo deseado de un nodo; el agente lo recibe en la próxima respuesta.
    pub fn set_mode(&self, id: NodeId, mode: Mode, now: Instant) -> bool {
        let mut nodes = self.write();
        let Some(e) = nodes.get_mut(&id) else {
            return false;
        };
        if e.mode != mode {
            e.mode = mode;
            tracing::info!(node = %e.name, ?mode, "modo cambiado");
            self.refresh(e, now);
            self.emit(e, now);
        }
        true
    }

    /// Quita un nodo sin esperar a que esté Offline (la cabeza del modo dividido).
    pub fn remove(&self, id: NodeId) {
        if self.write().remove(&id).is_some() {
            self.events.publish(Event::NodeRemoved(id));
        }
    }

    /// Encola un comando para el nodo; sale en la respuesta al próximo heartbeat.
    pub fn queue_command(&self, id: NodeId, kind: CommandKind, now: Instant) -> Option<Command> {
        let mut nodes = self.write();
        let e = nodes.get_mut(&id)?;
        let cmd = Command {
            id: uuid::Uuid::new_v4().to_string(),
            kind,
        };
        e.pending.push(cmd.clone());
        tracing::info!(node = %e.name, command = %cmd.id, ?kind, "comando encolado");
        self.emit(e, now);
        Some(cmd)
    }

    /// Registra un nodo con `proto_version` desconocido para mostrarlo.
    pub fn incompatible(&self, header: &HeartbeatHeader, reason: String, now: Instant) {
        let name = header.name.clone().unwrap_or_else(|| "desconocido".into());
        let id = header
            .node_id
            .unwrap_or_else(|| NodeId::new_v5(&NodeId::NAMESPACE_OID, name.as_bytes()));
        let mut nodes = self.write();
        let e = nodes
            .entry(id)
            .or_insert_with(|| NodeEntry::new(id, name.clone(), now));
        e.name = name;
        e.heartbeat = None;
        e.incompatible = Some(reason);
        e.last_seen = now;
        self.refresh(e, now);
        self.emit(e, now);
    }

    /// Tarea periódica: recalcula estados, emite los que cambian y poda el
    /// historial de peticiones.
    pub fn tick(&self, now: Instant, now_ms: i64) {
        let mut nodes = self.write();
        for e in nodes.values_mut() {
            if self.refresh(e, now) {
                tracing::info!(node = %e.name, status = ?e.status, "cambio de estado");
                self.emit(e, now);
            }
        }
        drop(nodes);
        let mut recent = self.recent();
        while recent.front().is_some_and(|t| now_ms - t > 60_000) {
            recent.pop_front();
        }
    }

    pub fn nodes(&self, now: Instant) -> Vec<NodeView> {
        let mut list: Vec<NodeView> = self.read().values().map(|e| NodeView::of(e, now)).collect();
        list.sort_by(|a, b| a.name.cmp(&b.name).then(a.node_id.cmp(&b.node_id)));
        list
    }

    pub fn set_drain(&self, id: NodeId, drain: bool, now: Instant) -> Option<NodeView> {
        let mut nodes = self.write();
        let e = nodes.get_mut(&id)?;
        e.draining = drain;
        self.refresh(e, now);
        self.emit(e, now);
        tracing::info!(node = %e.name, drain, "drain cambiado");
        Some(NodeView::of(e, now))
    }

    pub fn forget(&self, id: NodeId, now: Instant) -> Result<(), ForgetError> {
        let mut nodes = self.write();
        let e = nodes.get_mut(&id).ok_or(ForgetError::NotFound)?;
        self.refresh(e, now);
        if e.status != NodeStatus::Offline {
            return Err(ForgetError::NotOffline);
        }
        nodes.remove(&id);
        self.events.publish(Event::NodeRemoved(id));
        Ok(())
    }

    /// Foto de los nodos para `policy::select`, con estados al día.
    pub fn candidates(&self, now: Instant) -> Vec<Candidate> {
        let mut nodes = self.write();
        nodes
            .values_mut()
            .map(|e| {
                if self.refresh(e, now) {
                    self.emit(e, now);
                }
                let hb = e.heartbeat.as_ref();
                Candidate {
                    id: e.id,
                    status: e.status,
                    model: hb.and_then(|h| h.llama.as_ref()?.model.clone()),
                    free_slots: free_slots(hb.and_then(|h| h.llama.as_ref()), e.in_flight),
                    in_flight: e.in_flight,
                    temp_c: hb.and_then(Heartbeat::max_temp_c),
                }
            })
            .collect()
    }

    /// Unión de modelos de los nodos Ready y Busy.
    pub fn models(&self) -> Vec<String> {
        let mut models: Vec<String> = self
            .read()
            .values()
            .filter(|e| e.status.accepts_requests())
            .filter_map(|e| e.heartbeat.as_ref()?.llama.as_ref()?.model.clone())
            .collect();
        models.sort();
        models.dedup();
        models
    }

    /// Marca el inicio de una petición hacia `id` (`in_flight += 1`).
    pub fn begin(&self, id: NodeId, now: Instant) -> Option<Target> {
        let mut nodes = self.write();
        let e = nodes.get_mut(&id)?;
        let url = e.heartbeat.as_ref()?.advertise_url.clone();
        e.in_flight += 1;
        self.refresh(e, now);
        self.emit(e, now);
        Some(Target {
            id,
            name: e.name.clone(),
            url,
        })
    }

    /// Marca el fin (o fallo) de una petición (`in_flight -= 1`).
    pub fn end(&self, id: NodeId, now: Instant) {
        let mut nodes = self.write();
        if let Some(e) = nodes.get_mut(&id) {
            e.in_flight = e.in_flight.saturating_sub(1);
            self.refresh(e, now);
            self.emit(e, now);
        }
    }

    /// Registra una petición terminada: contadores, tok/s y evento.
    pub fn record(&self, done: RequestDone, now: Instant) {
        {
            let mut nodes = self.write();
            if let Some(e) = nodes.get_mut(&done.node_id) {
                if (200..300).contains(&done.status) {
                    e.requests_served += 1;
                }
                if done.tok_s.is_some() {
                    e.last_tok_s = done.tok_s;
                }
                self.emit(e, now);
            }
        }
        self.recent().push_back(done.at_ms);
        self.events.publish(Event::RequestDone(done));
    }

    /// Fin de las peticiones del último minuto (ms Unix).
    pub fn recent_requests(&self, now_ms: i64) -> Vec<i64> {
        self.recent()
            .iter()
            .copied()
            .filter(|t| now_ms - t <= 60_000)
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use time::OffsetDateTime;

    pub fn thresholds() -> Thresholds {
        Thresholds {
            offline_after: Duration::from_secs(15),
            max_temp_c: 45.0,
            resume_temp_c: 40.0,
            max_soc_temp_c: 80.0,
            resume_soc_temp_c: 70.0,
            min_battery_pct: 20,
        }
    }

    pub fn hb(id: u128, temp: f32, pct: u8, charging: bool, busy: u32) -> Heartbeat {
        Heartbeat {
            proto_version: 1,
            node_id: NodeId::from_u128(id),
            name: format!("n{id}"),
            agent_version: "0.1.0".into(),
            advertise_url: format!("http://10.0.0.{id}:8080"),
            sent_at: OffsetDateTime::UNIX_EPOCH,
            // `temp` es la batería; el SoC va 10 °C por encima, como bajo carga.
            battery: Some(Battery {
                pct,
                charging,
                temp_c: Some(temp),
            }),
            thermal: Some(Thermal { max_c: temp + 10.0 }),
            memory: None,
            llama: Some(LlamaInfo {
                status: LlamaStatus::Ok,
                model: Some("m".into()),
                ctx_size: Some(4096),
                slots_total: Some(2),
                slots_busy: Some(busy),
            }),
            rpc: None,
            last_error: None,
            acked_commands: vec![],
        }
    }

    fn entry(h: Heartbeat, seen: Instant) -> NodeEntry {
        let mut e = NodeEntry::new(h.node_id, h.name.clone(), seen);
        e.heartbeat = Some(h);
        e
    }

    fn status(e: &NodeEntry, now: Instant) -> NodeStatus {
        let t = thresholds();
        let (battery, soc) = temps(e.heartbeat.as_ref());
        compute_status(e, hot_latch(battery, soc, e.hot, &t), now, &t)
    }

    #[test]
    fn ready() {
        let now = Instant::now();
        assert_eq!(
            status(&entry(hb(1, 35.0, 80, false, 0), now), now),
            NodeStatus::Ready
        );
    }

    #[test]
    fn offline_after_15s_wins_over_everything() {
        let now = Instant::now();
        let mut e = entry(hb(1, 50.0, 5, false, 2), now);
        e.draining = true;
        assert_eq!(
            status(&e, now + Duration::from_secs(15)),
            NodeStatus::Draining
        );
        assert_eq!(
            status(&e, now + Duration::from_millis(15_001)),
            NodeStatus::Offline
        );
    }

    #[test]
    fn draining_wins_over_down() {
        let now = Instant::now();
        let mut h = hb(1, 35.0, 80, false, 0);
        h.llama.as_mut().unwrap().status = LlamaStatus::Down;
        let mut e = entry(h, now);
        assert_eq!(status(&e, now), NodeStatus::Down);
        e.draining = true;
        assert_eq!(status(&e, now), NodeStatus::Draining);
    }

    #[test]
    fn down_when_llama_not_ok_or_missing() {
        let now = Instant::now();
        for s in [
            LlamaStatus::Loading,
            LlamaStatus::Error,
            LlamaStatus::Unknown,
        ] {
            let mut h = hb(1, 50.0, 5, false, 0);
            h.llama.as_mut().unwrap().status = s;
            assert_eq!(status(&entry(h, now), now), NodeStatus::Down);
        }
        let mut h = hb(1, 35.0, 80, false, 0);
        h.llama = None;
        assert_eq!(status(&entry(h, now), now), NodeStatus::Down);
    }

    #[test]
    fn hot_wins_over_low_battery() {
        let now = Instant::now();
        assert_eq!(
            status(&entry(hb(1, 45.0, 5, false, 2), now), now),
            NodeStatus::Hot
        );
    }

    #[test]
    fn soc_has_its_own_higher_threshold() {
        let now = Instant::now();
        let mut h = hb(1, 30.0, 80, true, 0);
        h.thermal = Some(Thermal { max_c: 79.0 });
        assert_eq!(
            status(&entry(h.clone(), now), now),
            NodeStatus::Ready,
            "SoC a 79 °C es normal bajo carga"
        );
        h.thermal = Some(Thermal { max_c: 80.0 });
        assert_eq!(status(&entry(h, now), now), NodeStatus::Hot);
    }

    #[test]
    fn hot_hysteresis() {
        let t = thresholds();
        // Batería
        assert!(!hot_latch(Some(44.9), None, false, &t));
        assert!(hot_latch(Some(45.0), None, false, &t));
        assert!(
            hot_latch(Some(42.0), None, true, &t),
            "entre 40 y 45 se queda Hot"
        );
        assert!(
            !hot_latch(Some(42.0), None, false, &t),
            "entre 40 y 45 no entra"
        );
        assert!(hot_latch(Some(40.0), None, true, &t));
        assert!(!hot_latch(Some(39.9), None, true, &t));
        // SoC
        assert!(hot_latch(None, Some(80.0), false, &t));
        assert!(hot_latch(None, Some(75.0), true, &t));
        assert!(!hot_latch(None, Some(69.9), true, &t));
        // Sale solo cuando los dos se han enfriado.
        assert!(hot_latch(Some(39.0), Some(75.0), true, &t));
        assert!(hot_latch(Some(42.0), Some(50.0), true, &t));
        assert!(!hot_latch(Some(39.0), Some(50.0), true, &t));
        // Sin lecturas conserva el valor.
        assert!(hot_latch(None, None, true, &t));
        assert!(!hot_latch(None, None, false, &t));
    }

    #[test]
    fn low_battery_only_when_not_charging() {
        let now = Instant::now();
        assert_eq!(
            status(&entry(hb(1, 35.0, 19, false, 0), now), now),
            NodeStatus::LowBattery
        );
        assert_eq!(
            status(&entry(hb(1, 35.0, 19, true, 0), now), now),
            NodeStatus::Ready
        );
        assert_eq!(
            status(&entry(hb(1, 35.0, 20, false, 0), now), now),
            NodeStatus::Ready
        );
        let mut h = hb(1, 35.0, 0, false, 0);
        h.battery = None;
        assert_eq!(status(&entry(h, now), now), NodeStatus::Ready);
    }

    #[test]
    fn low_battery_wins_over_busy() {
        let now = Instant::now();
        assert_eq!(
            status(&entry(hb(1, 35.0, 10, false, 2), now), now),
            NodeStatus::LowBattery
        );
    }

    #[test]
    fn busy_uses_max_of_reported_and_in_flight() {
        let now = Instant::now();
        assert_eq!(
            status(&entry(hb(1, 35.0, 80, false, 2), now), now),
            NodeStatus::Busy
        );
        let mut e = entry(hb(1, 35.0, 80, false, 0), now);
        e.in_flight = 2;
        assert_eq!(status(&e, now), NodeStatus::Busy);
        e.in_flight = 1;
        assert_eq!(status(&e, now), NodeStatus::Ready);
        e.in_flight = 5;
        assert_eq!(
            free_slots(e.heartbeat.as_ref().unwrap().llama.as_ref(), 5),
            0
        );
    }

    #[test]
    fn unknown_slots_assume_one() {
        assert_eq!(free_slots(None, 0), 1);
        assert_eq!(free_slots(None, 1), 0);
    }

    #[test]
    fn incompatible_and_without_heartbeat() {
        let now = Instant::now();
        let mut e = NodeEntry::new(NodeId::nil(), "x".into(), now);
        assert_eq!(status(&e, now), NodeStatus::Offline);
        e.incompatible = Some("proto_version 2".into());
        assert_eq!(status(&e, now), NodeStatus::Incompatible);
        assert_eq!(
            status(&e, now + Duration::from_secs(16)),
            NodeStatus::Offline
        );
    }

    #[test]
    fn registry_flow_drain_forget_and_hysteresis() {
        let r = Registry::new(thresholds());
        let t0 = Instant::now();
        let h = hb(1, 46.0, 80, true, 0);
        let id = h.node_id;
        r.heartbeat(h, t0);
        assert_eq!(r.nodes(t0)[0].status, NodeStatus::Hot);
        r.heartbeat(hb(1, 42.0, 80, true, 0), t0);
        assert_eq!(r.nodes(t0)[0].status, NodeStatus::Hot);
        r.heartbeat(hb(1, 39.0, 80, true, 0), t0);
        assert_eq!(r.nodes(t0)[0].status, NodeStatus::Ready);
        assert_eq!(r.models(), vec!["m".to_string()]);

        r.set_drain(id, true, t0).unwrap();
        assert_eq!(r.nodes(t0)[0].status, NodeStatus::Draining);
        assert!(r.models().is_empty());
        r.set_drain(id, false, t0).unwrap();

        assert_eq!(r.forget(id, t0), Err(ForgetError::NotOffline));
        assert_eq!(r.forget(NodeId::nil(), t0), Err(ForgetError::NotFound));
        let later = t0 + Duration::from_secs(16);
        r.tick(later, 0);
        assert_eq!(r.nodes(later)[0].status, NodeStatus::Offline);
        assert_eq!(r.forget(id, later), Ok(()));
        assert!(r.nodes(later).is_empty());
    }

    #[test]
    fn commands_are_redelivered_until_acked() {
        let r = Registry::new(thresholds());
        let t0 = Instant::now();
        let h = hb(1, 30.0, 80, true, 0);
        let id = h.node_id;
        assert!(r.heartbeat(h.clone(), t0).commands.is_empty());
        assert!(
            r.queue_command(NodeId::nil(), CommandKind::RestartLlama, t0)
                .is_none()
        );
        let cmd = r.queue_command(id, CommandKind::RestartLlama, t0).unwrap();
        assert_eq!(r.heartbeat(h.clone(), t0).commands, vec![cmd.clone()]);
        assert_eq!(
            r.heartbeat(h.clone(), t0).commands,
            vec![cmd.clone()],
            "sin ack se reenvía"
        );
        assert_eq!(
            r.nodes(t0)[0].pending_commands,
            vec![CommandKind::RestartLlama]
        );
        let mut acked = h;
        acked.acked_commands = vec![cmd.id];
        assert!(r.heartbeat(acked, t0).commands.is_empty());
        assert!(r.nodes(t0)[0].pending_commands.is_empty());
    }

    #[test]
    fn worker_mode_is_sent_and_changes_status() {
        let r = Registry::new(thresholds());
        let t0 = Instant::now();
        let mut h = hb(1, 30.0, 80, true, 0);
        let id = h.node_id;
        assert_eq!(r.heartbeat(h.clone(), t0).mode, Mode::Solo);
        assert!(r.set_mode(id, Mode::Worker, t0));
        assert!(!r.set_mode(NodeId::nil(), Mode::Worker, t0));
        // Aún con llama-server y sin rpc-server: Down, no recibe peticiones.
        assert_eq!(r.heartbeat(h.clone(), t0).mode, Mode::Worker);
        assert_eq!(r.nodes(t0)[0].status, NodeStatus::Down);
        h.rpc = Some(RpcInfo {
            port: 50052,
            running: true,
        });
        r.heartbeat(h.clone(), t0);
        assert_eq!(r.nodes(t0)[0].status, NodeStatus::Worker);
        assert!(r.models().is_empty());
        r.set_mode(id, Mode::Solo, t0);
        assert_eq!(r.nodes(t0)[0].status, NodeStatus::Ready);
        r.remove(id);
        assert!(r.nodes(t0).is_empty());
    }

    #[test]
    fn in_flight_counts_and_events() {
        let r = Registry::new(thresholds());
        let mut rx = r.events().subscribe();
        let t0 = Instant::now();
        let h = hb(1, 30.0, 80, true, 0);
        let id = h.node_id;
        r.heartbeat(h, t0);
        let target = r.begin(id, t0).unwrap();
        assert_eq!(target.url, "http://10.0.0.1:8080");
        r.begin(id, t0).unwrap();
        assert_eq!(r.nodes(t0)[0].status, NodeStatus::Busy);
        r.end(id, t0);
        assert_eq!(r.nodes(t0)[0].in_flight, 1);
        assert_eq!(r.nodes(t0)[0].status, NodeStatus::Ready);
        let mut names = vec![];
        while let Ok(ev) = rx.try_recv() {
            names.push(ev.name());
        }
        assert!(names.iter().all(|n| *n == "node_updated"));
        assert_eq!(names.len(), 4);
    }
}
