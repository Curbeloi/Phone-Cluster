//! Alta de teléfonos con la app Android: la app pide unirse, el Mac la acepta
//! con un clic en el dashboard y la app recibe lo que necesita (token, modelo,
//! argumentos de llama-server). Ver DECISIONS.md, D-42 y siguientes.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::events::{Event, EventBus};

/// Una solicitud pendiente desaparece si la app deja de preguntar.
const PENDING_TTL: Duration = Duration::from_secs(60);
/// Las decididas se recuerdan un rato para mostrarlas y responder a la app.
const DECIDED_TTL: Duration = Duration::from_secs(10 * 60);
/// `/api/v1/join` no lleva token: tope de solicitudes recordadas a la vez.
const MAX_ENTRIES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinState {
    Pending,
    Approved,
    Rejected,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JoinRequest {
    /// UUID que la app genera una vez y guarda.
    pub device_id: String,
    /// Modelo del teléfono (`Build.MODEL`), p. ej. "SM-A546B".
    pub model: String,
    /// Nombre comercial si la app lo conoce, p. ej. "Galaxy A54".
    #[serde(default)]
    pub label: Option<String>,
    /// Token de una aprobación anterior: si sigue siendo válido, no hace
    /// falta volver a aceptar el teléfono tras reiniciar el Mac.
    #[serde(default)]
    pub token: Option<String>,
}

impl JoinRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        let ok_id = !self.device_id.is_empty()
            && self.device_id.len() <= 64
            && self
                .device_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !ok_id {
            return Err("device_id inválido");
        }
        let text_ok = |s: &str| {
            !s.trim().is_empty() && s.chars().count() <= 64 && !s.chars().any(char::is_control)
        };
        if !text_ok(&self.model) {
            return Err("model inválido");
        }
        if self.label.as_deref().is_some_and(|l| !text_ok(l)) {
            return Err("label inválido");
        }
        Ok(())
    }
}

/// Lo que ve el dashboard de cada solicitud.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JoinView {
    pub device_id: String,
    pub model: String,
    pub label: Option<String>,
    pub ip: String,
    pub state: JoinState,
    /// Nombre que tendrá como nodo.
    pub node_name: String,
}

struct Entry {
    view: JoinView,
    last_seen: Instant,
}

#[derive(Default)]
pub struct Joins {
    map: Mutex<HashMap<String, Entry>>,
    events: Option<EventBus>,
}

impl Joins {
    pub fn new(events: EventBus) -> Self {
        Self {
            map: Mutex::default(),
            events: Some(events),
        }
    }

    fn map(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, view: &JoinView) {
        if let Some(ev) = &self.events {
            ev.publish(Event::JoinUpdated(Box::new(view.clone())));
        }
    }

    /// Registra (o refresca) una solicitud y devuelve su estado.
    /// `token_ok`: la app presentó el token vigente, así que queda aprobada.
    /// `None` si es nueva y ya hay demasiadas.
    pub fn request(
        &self,
        req: &JoinRequest,
        ip: IpAddr,
        token_ok: bool,
        now: Instant,
    ) -> Option<JoinView> {
        let node_name =
            crate::devices::node_name(req.label.as_deref().unwrap_or(&req.model), &ip.to_string());
        let mut map = self.map();
        let is_new = !map.contains_key(&req.device_id);
        if is_new && map.len() >= MAX_ENTRIES {
            return None;
        }
        let entry = map.entry(req.device_id.clone()).or_insert_with(|| Entry {
            view: JoinView {
                device_id: req.device_id.clone(),
                model: req.model.clone(),
                label: req.label.clone(),
                ip: ip.to_string(),
                state: JoinState::Pending,
                node_name: node_name.clone(),
            },
            last_seen: now,
        });
        let before = entry.view.clone();
        entry.last_seen = now;
        entry.view.ip = ip.to_string();
        entry.view.model = req.model.clone();
        entry.view.label = req.label.clone();
        entry.view.node_name = node_name;
        if token_ok {
            entry.view.state = JoinState::Approved;
        }
        let view = entry.view.clone();
        drop(map);
        if is_new || view != before {
            self.emit(&view);
            tracing::info!(device = %view.device_id, model = %view.model, ip = %view.ip, state = ?view.state, "solicitud de alta");
        }
        Some(view)
    }

    /// Acepta o rechaza desde el dashboard.
    pub fn decide(&self, device_id: &str, approve: bool) -> Option<JoinView> {
        let mut map = self.map();
        let entry = map.get_mut(device_id)?;
        entry.view.state = if approve {
            JoinState::Approved
        } else {
            JoinState::Rejected
        };
        let view = entry.view.clone();
        drop(map);
        tracing::info!(device = %view.device_id, model = %view.model, approve, "alta decidida");
        self.emit(&view);
        Some(view)
    }

    pub fn list(&self) -> Vec<JoinView> {
        let mut list: Vec<JoinView> = self.map().values().map(|e| e.view.clone()).collect();
        list.sort_by(|a, b| a.model.cmp(&b.model).then(a.device_id.cmp(&b.device_id)));
        list
    }

    /// Olvida las solicitudes de apps que dejaron de preguntar.
    pub fn prune(&self, now: Instant) {
        let mut removed = Vec::new();
        self.map().retain(|id, e| {
            let ttl = match e.view.state {
                JoinState::Pending => PENDING_TTL,
                _ => DECIDED_TTL,
            };
            let keep = now.saturating_duration_since(e.last_seen) <= ttl;
            if !keep {
                removed.push(id.clone());
            }
            keep
        });
        if let Some(ev) = &self.events {
            for id in removed {
                ev.publish(Event::JoinRemoved(id));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(id: &str) -> JoinRequest {
        JoinRequest {
            device_id: id.into(),
            model: "SM-A546B".into(),
            label: Some("Galaxy A54".into()),
            token: None,
        }
    }

    fn ip() -> IpAddr {
        IpAddr::from([192, 168, 100, 234])
    }

    #[test]
    fn validates_requests() {
        assert!(
            req("8f0c2a4e-1b7d-4c3e-9a51-0d6f2b8e7c10")
                .validate()
                .is_ok()
        );
        assert!(req("").validate().is_err());
        assert!(req("a b").validate().is_err());
        assert!(req(&"x".repeat(65)).validate().is_err());
        let mut r = req("abc");
        r.model = " ".into();
        assert!(r.validate().is_err());
    }

    #[test]
    fn pending_until_decided() {
        let j = Joins::default();
        let t0 = Instant::now();
        let v = j.request(&req("a"), ip(), false, t0).unwrap();
        assert_eq!(v.state, JoinState::Pending);
        assert_eq!(v.node_name, "galaxy-a54-234");
        assert_eq!(
            j.request(&req("a"), ip(), false, t0).unwrap().state,
            JoinState::Pending
        );
        assert_eq!(j.decide("a", true).unwrap().state, JoinState::Approved);
        assert_eq!(
            j.request(&req("a"), ip(), false, t0).unwrap().state,
            JoinState::Approved
        );
        assert_eq!(j.decide("a", false).unwrap().state, JoinState::Rejected);
        assert!(j.decide("nope", true).is_none());
        assert_eq!(j.list().len(), 1);
    }

    #[test]
    fn valid_token_skips_approval() {
        let j = Joins::default();
        let v = j.request(&req("a"), ip(), true, Instant::now()).unwrap();
        assert_eq!(v.state, JoinState::Approved);
    }

    #[test]
    fn caps_entries_and_rejects_bad_labels() {
        let j = Joins::default();
        let t0 = Instant::now();
        for i in 0..MAX_ENTRIES {
            assert!(j.request(&req(&format!("d{i}")), ip(), false, t0).is_some());
        }
        assert!(j.request(&req("una-mas"), ip(), false, t0).is_none());
        assert!(
            j.request(&req("d0"), ip(), false, t0).is_some(),
            "las conocidas siguen"
        );
        let mut r = req("x");
        r.label = Some("a\nb".into());
        assert!(r.validate().is_err());
        r.label = Some("x".repeat(65));
        assert!(r.validate().is_err());
    }

    #[test]
    fn prune_forgets_silent_apps() {
        let j = Joins::default();
        let t0 = Instant::now();
        j.request(&req("p"), ip(), false, t0);
        j.request(&req("d"), ip(), false, t0);
        j.decide("d", true);
        j.prune(t0 + Duration::from_secs(61));
        let ids: Vec<String> = j.list().into_iter().map(|v| v.device_id).collect();
        assert_eq!(ids, vec!["d".to_string()]);
        j.prune(t0 + Duration::from_secs(601));
        assert!(j.list().is_empty());
    }
}
