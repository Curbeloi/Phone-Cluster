//! Tipos del protocolo entre `pc-agent` y `pc-coordinator`.
//!
//! Sin I/O: solo tipos serde y validación. Ambos binarios dependen de este
//! crate para que el formato del heartbeat no pueda divergir.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Versión del protocolo que hablan este agente y este coordinador.
pub const PROTO_VERSION: u32 = 1;

/// Identificador estable de un nodo (teléfono).
pub type NodeId = Uuid;

/// Foto del teléfono y de su `llama-server` que el agente envía cada pocos segundos.
///
/// Cualquier bloque de métricas puede ser `null` si no se pudo leer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub proto_version: u32,
    pub node_id: NodeId,
    pub name: String,
    pub agent_version: String,
    pub advertise_url: String,
    #[serde(with = "time::serde::rfc3339")]
    pub sent_at: OffsetDateTime,
    pub battery: Option<Battery>,
    pub thermal: Option<Thermal>,
    pub memory: Option<Memory>,
    pub llama: Option<LlamaInfo>,
    /// `rpc-server` del modo dividido; `None` si el agente no sabe lanzarlo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc: Option<RpcInfo>,
    /// Por qué murió el último proceso supervisado (señal o código y la
    /// última línea de su log). Se borra cuando el proceso aguanta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Ids de comandos ejecutados desde el heartbeat anterior (Fase 5).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acked_commands: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Battery {
    pub pct: u8,
    pub charging: bool,
    pub temp_c: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thermal {
    pub max_c: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub total_mb: u64,
    pub available_mb: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlamaInfo {
    pub status: LlamaStatus,
    pub model: Option<String>,
    pub ctx_size: Option<u32>,
    pub slots_total: Option<u32>,
    pub slots_busy: Option<u32>,
}

/// `rpc-server` de llama.cpp: expone la CPU del teléfono para que otro
/// `llama-server` le cargue capas del modelo (modo dividido, D-56).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcInfo {
    pub port: u16,
    /// El proceso está lanzado.
    pub running: bool,
}

/// Estado del `llama-server` según `GET /health`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlamaStatus {
    /// 200 en `/health`.
    Ok,
    /// 503 "Loading model".
    Loading,
    /// Respondió, pero con un error.
    Error,
    /// No responde.
    Down,
    /// Valor desconocido enviado por un agente más nuevo.
    #[serde(other)]
    Unknown,
}

/// Cuerpo de la respuesta a `POST /api/v1/heartbeat`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    #[serde(default)]
    pub commands: Vec<Command>,
    /// Qué debe ejecutar el teléfono. Es un estado, no una orden: se repite
    /// en cada respuesta y aplicarlo dos veces no cambia nada.
    #[serde(default)]
    pub mode: Mode,
}

/// Qué proceso corre el teléfono.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// `llama-server` con el modelo completo.
    #[default]
    Solo,
    /// `rpc-server`: guarda y calcula capas para la cabeza del modo dividido.
    Worker,
}

/// Orden del coordinador al agente (Fase 5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Command {
    pub id: String,
    pub kind: CommandKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    RestartLlama,
    #[serde(other)]
    Unknown,
}

/// Estado de un nodo calculado por el coordinador.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeStatus {
    Ready,
    Busy,
    Hot,
    LowBattery,
    Draining,
    Down,
    Offline,
    /// Envió un `proto_version` que este coordinador no entiende.
    Incompatible,
    /// Presta sus capas al modo dividido; no recibe peticiones directas.
    Worker,
}

impl NodeStatus {
    /// Estados en los que el nodo puede recibir peticiones de inferencia.
    pub fn accepts_requests(self) -> bool {
        matches!(self, NodeStatus::Ready | NodeStatus::Busy)
    }
}

/// Lectura mínima de un heartbeat, tolerante a cualquier versión.
///
/// El coordinador la usa para identificar al nodo incluso cuando el
/// `proto_version` no es compatible y el resto del cuerpo no deserializa.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HeartbeatHeader {
    pub proto_version: Option<u32>,
    pub node_id: Option<NodeId>,
    pub name: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ProtoError {
    #[error("proto_version {0} no soportada (se espera {PROTO_VERSION})")]
    UnsupportedVersion(u32),
    #[error("name vacío o de más de 64 caracteres")]
    InvalidName,
    #[error("advertise_url debe empezar por http:// o https://")]
    InvalidAdvertiseUrl,
    #[error("battery.pct fuera de rango: {0}")]
    InvalidBatteryPct(u8),
}

impl Heartbeat {
    /// Comprueba lo que serde no puede expresar en los tipos.
    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.proto_version != PROTO_VERSION {
            return Err(ProtoError::UnsupportedVersion(self.proto_version));
        }
        if self.name.trim().is_empty() || self.name.chars().count() > 64 {
            return Err(ProtoError::InvalidName);
        }
        if !(self.advertise_url.starts_with("http://")
            || self.advertise_url.starts_with("https://"))
        {
            return Err(ProtoError::InvalidAdvertiseUrl);
        }
        if let Some(b) = &self.battery
            && b.pct > 100
        {
            return Err(ProtoError::InvalidBatteryPct(b.pct));
        }
        Ok(())
    }

    /// Temperatura más alta conocida del teléfono: SoC o batería.
    pub fn max_temp_c(&self) -> Option<f32> {
        let soc = self.thermal.as_ref().map(|t| t.max_c);
        let bat = self.battery.as_ref().and_then(|b| b.temp_c);
        match (soc, bat) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// El ejemplo de la especificación, con un UUID completo en `node_id`.
    const SPEC_EXAMPLE: &str = r#"{
      "proto_version": 1,
      "node_id": "8f0c2a4e-1b7d-4c3e-9a51-0d6f2b8e7c10",
      "name": "pixel-7",
      "agent_version": "0.1.0",
      "advertise_url": "http://192.168.1.21:8080",
      "sent_at": "2026-10-28T15:04:05Z",
      "battery": { "pct": 82, "charging": true, "temp_c": 34.5 },
      "thermal": { "max_c": 41.2 },
      "memory": { "total_mb": 7680, "available_mb": 2100 },
      "llama": {
        "status": "ok",
        "model": "qwen2.5-3b-instruct-q4_k_m",
        "ctx_size": 4096,
        "slots_total": 2,
        "slots_busy": 1
      }
    }"#;

    #[test]
    fn spec_example_deserializes() {
        let hb: Heartbeat = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        assert_eq!(hb.proto_version, 1);
        assert_eq!(hb.name, "pixel-7");
        assert_eq!(hb.battery.as_ref().unwrap().pct, 82);
        assert_eq!(hb.thermal.as_ref().unwrap().max_c, 41.2);
        assert_eq!(hb.memory.as_ref().unwrap().available_mb, 2100);
        let llama = hb.llama.as_ref().unwrap();
        assert_eq!(llama.status, LlamaStatus::Ok);
        assert_eq!(llama.slots_total, Some(2));
        assert_eq!(hb.sent_at.unix_timestamp(), 1_793_199_845);
        assert!(hb.acked_commands.is_empty());
        hb.validate().unwrap();
    }

    #[test]
    fn roundtrip_preserves_everything() {
        let hb: Heartbeat = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        let text = serde_json::to_string(&hb).unwrap();
        let back: Heartbeat = serde_json::from_str(&text).unwrap();
        assert_eq!(hb, back);
        assert!(text.contains("\"sent_at\":\"2026-10-28T15:04:05Z\""));
        assert!(!text.contains("acked_commands"));
    }

    #[test]
    fn metric_blocks_may_be_null_or_missing() {
        let json = r#"{
          "proto_version": 1,
          "node_id": "8f0c2a4e-1b7d-4c3e-9a51-0d6f2b8e7c10",
          "name": "x", "agent_version": "0.1.0",
          "advertise_url": "http://10.0.0.2:8080",
          "sent_at": "2026-10-28T15:04:05Z",
          "battery": null, "thermal": null
        }"#;
        let hb: Heartbeat = serde_json::from_str(json).unwrap();
        assert!(hb.battery.is_none() && hb.thermal.is_none());
        assert!(hb.memory.is_none() && hb.llama.is_none());
        assert_eq!(hb.max_temp_c(), None);
    }

    #[test]
    fn llama_fields_are_optional_and_unknown_status_is_tolerated() {
        let info: LlamaInfo = serde_json::from_str(r#"{"status":"sleeping"}"#).unwrap();
        assert_eq!(info.status, LlamaStatus::Unknown);
        assert_eq!(info.model, None);
        assert_eq!(info.slots_busy, None);
    }

    #[test]
    fn validate_rejects_unknown_version_and_bad_fields() {
        let mut hb: Heartbeat = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        hb.proto_version = 2;
        assert_eq!(hb.validate(), Err(ProtoError::UnsupportedVersion(2)));
        hb.proto_version = 1;
        hb.name = "  ".into();
        assert_eq!(hb.validate(), Err(ProtoError::InvalidName));
        hb.name = "ok".into();
        hb.advertise_url = "192.168.1.2:8080".into();
        assert_eq!(hb.validate(), Err(ProtoError::InvalidAdvertiseUrl));
        hb.advertise_url = "http://192.168.1.2:8080".into();
        hb.battery.as_mut().unwrap().pct = 101;
        assert_eq!(hb.validate(), Err(ProtoError::InvalidBatteryPct(101)));
    }

    #[test]
    fn header_reads_any_version() {
        let h: HeartbeatHeader =
            serde_json::from_str(r#"{"proto_version":7,"name":"future","whatever":[1,2]}"#)
                .unwrap();
        assert_eq!(h.proto_version, Some(7));
        assert_eq!(h.name.as_deref(), Some("future"));
        assert_eq!(h.node_id, None);
    }

    #[test]
    fn max_temp_uses_hottest_source() {
        let mut hb: Heartbeat = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        assert_eq!(hb.max_temp_c(), Some(41.2));
        hb.thermal = None;
        assert_eq!(hb.max_temp_c(), Some(34.5));
    }

    #[test]
    fn heartbeat_response_and_commands() {
        let r: HeartbeatResponse = serde_json::from_str(r#"{"commands":[]}"#).unwrap();
        assert!(r.commands.is_empty());
        let r: HeartbeatResponse = serde_json::from_str(
            r#"{"commands":[{"id":"c1","kind":"restart_llama"},{"id":"c2","kind":"reboot"}]}"#,
        )
        .unwrap();
        assert_eq!(r.commands[0].kind, CommandKind::RestartLlama);
        assert_eq!(r.commands[1].kind, CommandKind::Unknown);
        assert_eq!(r.mode, Mode::Solo, "sin mode, el de siempre");
        let r: HeartbeatResponse = serde_json::from_str(r#"{"mode":"worker"}"#).unwrap();
        assert_eq!(r.mode, Mode::Worker);
    }

    #[test]
    fn rpc_is_optional_and_omitted_when_absent() {
        let mut hb: Heartbeat = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        assert_eq!(hb.rpc, None);
        assert!(!serde_json::to_string(&hb).unwrap().contains("rpc"));
        hb.rpc = Some(RpcInfo {
            port: 50052,
            running: true,
        });
        let back: Heartbeat = serde_json::from_str(&serde_json::to_string(&hb).unwrap()).unwrap();
        assert_eq!(back.rpc, hb.rpc);
    }

    #[test]
    fn node_status_serializes_as_dashboard_labels() {
        assert_eq!(
            serde_json::to_string(&NodeStatus::LowBattery).unwrap(),
            "\"LowBattery\""
        );
        assert!(NodeStatus::Busy.accepts_requests());
        assert!(!NodeStatus::Hot.accepts_requests());
        assert!(!NodeStatus::Worker.accepts_requests());
    }
}
