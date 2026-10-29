//! Envío periódico del heartbeat con backoff exponencial ante fallos.

use std::collections::VecDeque;
use std::time::Duration;

use anyhow::{Context, bail};
use pc_proto::{
    Command, CommandKind, Heartbeat, HeartbeatResponse, Mode, NodeId, PROTO_VERSION, RpcInfo,
};
use reqwest::StatusCode;
use time::OffsetDateTime;

use crate::config::Config;
use crate::metrics::{Cache, Snapshot};
use crate::supervisor;

const MAX_BACKOFF: Duration = Duration::from_secs(30);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Identidad del nodo que va en cada heartbeat.
#[derive(Debug, Clone)]
pub struct Identity {
    pub node_id: NodeId,
    pub name: String,
    pub advertise_url: String,
}

pub fn build(id: &Identity, snap: &Snapshot, now: OffsetDateTime) -> Heartbeat {
    Heartbeat {
        proto_version: PROTO_VERSION,
        node_id: id.node_id,
        name: id.name.clone(),
        agent_version: env!("CARGO_PKG_VERSION").into(),
        advertise_url: id.advertise_url.clone(),
        sent_at: now.replace_nanosecond(0).unwrap_or(now),
        battery: snap.battery.clone(),
        thermal: snap.thermal.clone(),
        memory: snap.memory.clone(),
        llama: Some(snap.llama_info()),
        rpc: None,
        last_error: None,
        acked_commands: Vec::new(),
    }
}

/// Espera tras un fallo: el doble de la anterior, entre `base` y 30 s.
pub fn next_backoff(prev: Duration, base: Duration) -> Duration {
    (prev * 2).clamp(base, MAX_BACKOFF.max(base))
}

/// Comandos ya ejecutados (para no repetirlos si el ack se pierde) y
/// pendientes de confirmar en el próximo heartbeat.
#[derive(Debug, Default)]
pub struct Acks {
    done: VecDeque<String>,
    pending: Vec<String>,
}

impl Acks {
    const REMEMBER: usize = 64;

    /// Devuelve los comandos que aún no se han ejecutado y anota todos para confirmarlos.
    pub fn take_new(&mut self, commands: Vec<Command>) -> Vec<Command> {
        let mut new = Vec::new();
        for cmd in commands {
            if !self.pending.contains(&cmd.id) {
                self.pending.push(cmd.id.clone());
            }
            if self.done.contains(&cmd.id) {
                continue;
            }
            self.done.push_back(cmd.id.clone());
            if self.done.len() > Self::REMEMBER {
                self.done.pop_front();
            }
            new.push(cmd);
        }
        new
    }
}

/// Envía heartbeats para siempre. Los fallos se registran y se reintentan.
pub async fn run(
    cfg: &Config,
    id: &Identity,
    client: &reqwest::Client,
    cache: &Cache,
    supervisor: Option<&supervisor::Handle>,
) {
    let base = Duration::from_secs(cfg.heartbeat_secs);
    let url = format!(
        "{}/api/v1/heartbeat",
        cfg.coordinator_url.trim_end_matches('/')
    );
    let mut delay = base;
    let mut failing = false;
    let mut acks = Acks::default();
    let mut mode = Mode::Solo;
    let rpc_port = (!cfg.rpc_command.is_empty()).then_some(cfg.rpc_port);
    loop {
        let mut hb = build(id, &*cache.read().await, OffsetDateTime::now_utc());
        hb.acked_commands = acks.pending.clone();
        hb.rpc = rpc_info(rpc_port, supervisor);
        hb.last_error = supervisor.and_then(|h| h.state.borrow().last_error.clone());
        match send(client, &url, cfg.token.expose(), &hb).await {
            Ok(resp) => {
                if failing {
                    tracing::info!("coordinador alcanzable de nuevo");
                }
                failing = false;
                delay = base;
                // Los acks enviados con éxito ya no hace falta repetirlos.
                acks.pending.retain(|a| !hb.acked_commands.contains(a));
                for cmd in acks.take_new(resp.commands) {
                    execute(&cmd, supervisor).await;
                }
                if resp.mode != mode {
                    mode = resp.mode;
                    apply_mode(mode, rpc_port.is_some(), supervisor).await;
                }
            }
            Err(e) => {
                delay = if failing {
                    next_backoff(delay, base)
                } else {
                    base
                };
                failing = true;
                tracing::warn!(
                    error = format!("{e:#}"),
                    retry_in_s = delay.as_secs(),
                    "heartbeat falló"
                );
            }
        }
        tokio::time::sleep(delay).await;
    }
}

fn rpc_info(port: Option<u16>, supervisor: Option<&supervisor::Handle>) -> Option<RpcInfo> {
    let running = supervisor.is_some_and(|h| h.state.borrow().running == Some(Mode::Worker));
    port.map(|port| RpcInfo { port, running })
}

async fn apply_mode(mode: Mode, can_work: bool, supervisor: Option<&supervisor::Handle>) {
    match (mode, supervisor) {
        (Mode::Worker, _) if !can_work => {
            tracing::warn!("modo dividido pedido, pero falta rpc_command; sigue en modo solo");
        }
        (_, Some(h)) => {
            tracing::info!(?mode, "el coordinador cambió el modo");
            if h.tx.send(supervisor::Msg::SetMode(mode)).await.is_err() {
                tracing::error!("el supervisor no está activo");
            }
        }
        (_, None) => tracing::warn!(?mode, "modo ignorado: el agente no supervisa procesos"),
    }
}

async fn execute(cmd: &Command, supervisor: Option<&supervisor::Handle>) {
    match (cmd.kind, supervisor) {
        (CommandKind::RestartLlama, Some(h)) => {
            tracing::info!(id = %cmd.id, "ejecutando restart_llama");
            if h.tx.send(supervisor::Msg::Restart).await.is_err() {
                tracing::error!("el supervisor de llama-server no está activo");
            }
        }
        (CommandKind::RestartLlama, None) => {
            tracing::warn!(id = %cmd.id, "restart_llama ignorado: el agente no supervisa llama-server (falta llama_command)");
        }
        (CommandKind::Unknown, _) => {
            tracing::warn!(id = %cmd.id, "comando desconocido ignorado");
        }
    }
}

async fn send(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    hb: &Heartbeat,
) -> anyhow::Result<HeartbeatResponse> {
    let res = client
        .post(url)
        .bearer_auth(token)
        .json(hb)
        .timeout(SEND_TIMEOUT)
        .send()
        .await
        .context("coordinador inalcanzable")?;
    match res.status() {
        s if s.is_success() => Ok(res.json().await.unwrap_or_default()),
        StatusCode::UNAUTHORIZED => bail!("el coordinador rechazó el token (401)"),
        s => {
            let body = res.text().await.unwrap_or_default();
            bail!(
                "el coordinador respondió {s}: {}",
                body.chars().take(300).collect::<String>()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_30s() {
        let base = Duration::from_secs(5);
        let mut d = base;
        let mut seen = vec![];
        for _ in 0..5 {
            d = next_backoff(d, base);
            seen.push(d.as_secs());
        }
        assert_eq!(seen, vec![10, 20, 30, 30, 30]);
    }

    #[test]
    fn acks_run_each_command_once() {
        let cmd = |id: &str| Command {
            id: id.into(),
            kind: CommandKind::RestartLlama,
        };
        let mut acks = Acks::default();
        assert_eq!(acks.take_new(vec![cmd("a")]).len(), 1);
        // Reenviado porque el ack no llegó: se confirma otra vez, sin ejecutar.
        assert!(acks.take_new(vec![cmd("a")]).is_empty());
        assert_eq!(acks.pending, vec!["a".to_string()]);
        assert_eq!(acks.take_new(vec![cmd("a"), cmd("b")]).len(), 1);
        assert_eq!(acks.pending.len(), 2);
    }

    #[test]
    fn backoff_never_below_base() {
        let base = Duration::from_secs(60);
        assert_eq!(next_backoff(Duration::from_secs(1), base), base);
    }

    #[test]
    fn built_heartbeat_is_valid_even_without_metrics() {
        let id = Identity {
            node_id: uuid::Uuid::nil(),
            name: "n".into(),
            advertise_url: "http://10.0.0.2:8080".into(),
        };
        let hb = build(&id, &Snapshot::default(), OffsetDateTime::now_utc());
        hb.validate().unwrap();
        assert!(hb.battery.is_none());
        assert_eq!(hb.llama.unwrap().status, pc_proto::LlamaStatus::Down);
        assert_eq!(hb.sent_at.nanosecond(), 0);
    }
}
