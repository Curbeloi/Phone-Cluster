//! Eventos que el dashboard recibe por SSE en `/api/v1/events`.

use pc_proto::NodeId;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::devices::DeviceView;
use crate::join::JoinView;
use crate::registry::NodeView;
use crate::split::SplitView;

/// Resultado de una petición de inferencia atendida.
#[derive(Debug, Clone, Serialize)]
pub struct RequestDone {
    pub node_id: NodeId,
    pub node_name: String,
    pub endpoint: &'static str,
    pub stream: bool,
    pub status: u16,
    pub duration_ms: u64,
    pub tokens: Option<u64>,
    pub tok_s: Option<f64>,
    /// Momento en que terminó, en milisegundos Unix.
    pub at_ms: i64,
}

#[derive(Debug, Clone)]
pub enum Event {
    NodeUpdated(Box<NodeView>),
    NodeRemoved(NodeId),
    RequestDone(RequestDone),
    DeviceUpdated(Box<DeviceView>),
    DeviceRemoved(String),
    JoinUpdated(Box<JoinView>),
    JoinRemoved(String),
    SplitUpdated(Box<SplitView>),
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::NodeUpdated(_) => "node_updated",
            Event::NodeRemoved(_) => "node_removed",
            Event::RequestDone(_) => "request_done",
            Event::DeviceUpdated(_) => "device_updated",
            Event::DeviceRemoved(_) => "device_removed",
            Event::JoinUpdated(_) => "join_updated",
            Event::JoinRemoved(_) => "join_removed",
            Event::SplitUpdated(_) => "split_updated",
        }
    }

    pub fn data(&self) -> String {
        let value = match self {
            Event::NodeUpdated(v) => serde_json::to_value(v),
            Event::NodeRemoved(id) => Ok(serde_json::json!({ "node_id": id })),
            Event::RequestDone(r) => serde_json::to_value(r),
            Event::DeviceUpdated(d) => serde_json::to_value(d),
            Event::DeviceRemoved(key) => Ok(serde_json::json!({ "key": key })),
            Event::JoinUpdated(j) => serde_json::to_value(j),
            Event::JoinRemoved(id) => Ok(serde_json::json!({ "device_id": id })),
            Event::SplitUpdated(v) => serde_json::to_value(v),
        };
        value.map(|v| v.to_string()).unwrap_or_else(|_| "{}".into())
    }
}

#[derive(Debug, Clone)]
pub struct EventBus(broadcast::Sender<Event>);

impl Default for EventBus {
    fn default() -> Self {
        Self(broadcast::channel(1024).0)
    }
}

impl EventBus {
    /// Sin suscriptores el evento se descarta.
    pub fn publish(&self, event: Event) {
        let _ = self.0.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.0.subscribe()
    }
}
