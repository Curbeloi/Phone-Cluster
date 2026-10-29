//! Política de selección de nodo. Función pura, sin I/O ni reloj.

use std::cmp::Ordering;

use pc_proto::{NodeId, NodeStatus};

/// Lo que la política necesita saber de cada nodo.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub id: NodeId,
    pub status: NodeStatus,
    pub model: Option<String>,
    pub free_slots: u32,
    pub in_flight: u32,
    pub temp_c: Option<f32>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RequestInfo<'a> {
    /// `None`, `""` o `"auto"` aceptan cualquier modelo.
    pub model: Option<&'a str>,
    /// Nodos ya intentados en esta petición.
    pub exclude: &'a [NodeId],
}

/// Elige el nodo para una petición:
///
/// 1. nodos Ready cuyo modelo coincide (o `auto`);
/// 2. si no hay, nodos Busy con el mismo filtro;
/// 3. orden: más slots libres, menos `in_flight`, menos temperatura, `node_id`.
pub fn select(nodes: &[Candidate], req: &RequestInfo<'_>) -> Option<NodeId> {
    let best = |status: NodeStatus| {
        nodes
            .iter()
            .filter(|c| c.status == status)
            .filter(|c| !req.exclude.contains(&c.id))
            .filter(|c| model_matches(req.model, c.model.as_deref()))
            .min_by(|a, b| order(a, b))
            .map(|c| c.id)
    };
    best(NodeStatus::Ready).or_else(|| best(NodeStatus::Busy))
}

fn model_matches(wanted: Option<&str>, have: Option<&str>) -> bool {
    match wanted {
        None | Some("" | "auto") => true,
        Some(w) => have == Some(w),
    }
}

fn order(a: &Candidate, b: &Candidate) -> Ordering {
    // Sin temperatura conocida va detrás de cualquier lectura real.
    let temp = |c: &Candidate| c.temp_c.unwrap_or(f32::INFINITY);
    b.free_slots
        .cmp(&a.free_slots)
        .then(a.in_flight.cmp(&b.in_flight))
        .then(temp(a).total_cmp(&temp(b)))
        .then(a.id.cmp(&b.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: u128, status: NodeStatus) -> Candidate {
        Candidate {
            id: NodeId::from_u128(id),
            status,
            model: Some("qwen".into()),
            free_slots: 2,
            in_flight: 0,
            temp_c: Some(35.0),
        }
    }

    fn pick(nodes: &[Candidate]) -> Option<u128> {
        select(nodes, &RequestInfo::default()).map(|id| id.as_u128())
    }

    #[test]
    fn each_state_row() {
        use NodeStatus::*;
        for (status, chosen) in [
            (Offline, false),
            (Draining, false),
            (Down, false),
            (Hot, false),
            (LowBattery, false),
            (Incompatible, false),
            (Busy, true),
            (Ready, true),
        ] {
            assert_eq!(pick(&[c(1, status)]).is_some(), chosen, "{status:?}");
        }
    }

    #[test]
    fn busy_only_when_no_ready() {
        let mut busy = c(1, NodeStatus::Busy);
        busy.free_slots = 0;
        let ready = c(2, NodeStatus::Ready);
        assert_eq!(pick(&[busy.clone(), ready]), Some(2));
        assert_eq!(pick(&[busy, c(3, NodeStatus::Hot)]), Some(1));
    }

    #[test]
    fn model_filter_and_auto() {
        let mut a = c(1, NodeStatus::Ready);
        a.model = Some("phi".into());
        let b = c(2, NodeStatus::Ready);
        let nodes = [a, b];
        let req = |m| RequestInfo {
            model: Some(m),
            exclude: &[],
        };
        assert_eq!(select(&nodes, &req("phi")).map(|i| i.as_u128()), Some(1));
        assert_eq!(select(&nodes, &req("qwen")).map(|i| i.as_u128()), Some(2));
        assert_eq!(select(&nodes, &req("llama")), None);
        assert_eq!(select(&nodes, &req("auto")).map(|i| i.as_u128()), Some(1));
    }

    #[test]
    fn model_filter_applies_to_busy_fallback() {
        let mut busy = c(1, NodeStatus::Busy);
        busy.model = Some("phi".into());
        let req = RequestInfo {
            model: Some("qwen"),
            exclude: &[],
        };
        assert_eq!(select(&[busy], &req), None);
    }

    #[test]
    fn ordering_free_slots_then_in_flight_then_temp_then_id() {
        let mut a = c(1, NodeStatus::Ready);
        let mut b = c(2, NodeStatus::Ready);
        a.free_slots = 1;
        assert_eq!(pick(&[a.clone(), b.clone()]), Some(2), "más slots libres");

        a.free_slots = 2;
        b.in_flight = 1;
        assert_eq!(pick(&[a.clone(), b.clone()]), Some(1), "menos in_flight");

        b.in_flight = 0;
        a.temp_c = Some(40.0);
        assert_eq!(pick(&[a.clone(), b.clone()]), Some(2), "más fresco");

        a.temp_c = None;
        assert_eq!(
            pick(&[a.clone(), b.clone()]),
            Some(2),
            "sin temperatura va detrás"
        );

        a.temp_c = Some(35.0);
        assert_eq!(
            pick(&[b.clone(), a.clone()]),
            Some(1),
            "desempate por node_id"
        );
    }

    #[test]
    fn exclude_skips_tried_nodes() {
        let nodes = [c(1, NodeStatus::Ready), c(2, NodeStatus::Ready)];
        let tried = [NodeId::from_u128(1)];
        let req = RequestInfo {
            model: None,
            exclude: &tried,
        };
        assert_eq!(select(&nodes, &req).map(|i| i.as_u128()), Some(2));
    }

    #[test]
    fn no_candidates() {
        assert_eq!(pick(&[]), None);
    }
}
