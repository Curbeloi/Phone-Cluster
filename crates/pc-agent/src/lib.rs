//! `pc-agent`: observa el teléfono y su `llama-server` y envía heartbeats al
//! coordinador. No hace inferencia.

pub mod config;
pub mod heartbeat;
pub mod metrics;
pub mod supervisor;

use std::net::{ToSocketAddrs, UdpSocket};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use pc_proto::NodeId;
use reqwest::Url;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::config::Config;
use crate::heartbeat::Identity;
use crate::metrics::Cache;

/// Corre el agente hasta que se cancele la tarea (o, con `--once`, una vez).
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .build()
        .context("creando cliente HTTP")?;
    let id = Identity {
        node_id: node_id(&cfg)?,
        name: cfg.name.clone(),
        advertise_url: advertise_url(&cfg),
    };
    let cache = Cache::default();

    if cfg.once {
        metrics::collect_once(&cfg, &client, &cache).await;
        let hb = heartbeat::build(&id, &*cache.read().await, time::OffsetDateTime::now_utc());
        hb.validate()
            .context("el heartbeat generado no es válido")?;
        println!("{}", serde_json::to_string_pretty(&hb)?);
        return Ok(());
    }

    tracing::info!(
        node_id = %id.node_id,
        name = %id.name,
        coordinator = %cfg.coordinator_url,
        advertise_url = %id.advertise_url,
        mock = cfg.mock.is_some(),
        "pc-agent arrancado"
    );
    if cfg.token.expose().is_empty() {
        tracing::warn!("token vacío: el coordinador rechazará los heartbeats");
    }
    let cfg = Arc::new(cfg);
    // Primera foto completa antes del primer heartbeat; después, cada métrica a su ritmo.
    metrics::collect_once(&cfg, &client, &cache).await;
    let mut tasks = JoinSet::new();
    let solo = (!cfg.llama_command.is_empty()).then(|| supervisor::Proc {
        command: cfg.llama_command.clone(),
        log: cfg.state_dir.join("llama-server.log"),
        env: Vec::new(),
    });
    let worker = rpc_proc(&cfg, &id.advertise_url);
    let supervisor = if solo.is_none() && worker.is_none() {
        tracing::info!("sin llama_command: llama-server se gestiona fuera del agente");
        None
    } else {
        std::fs::create_dir_all(&cfg.state_dir).ok();
        Some(supervisor::spawn(solo, worker, &mut tasks))
    };
    metrics::spawn_all(cfg.clone(), client.clone(), cache.clone(), &mut tasks);
    tokio::select! {
        () = heartbeat::run(&cfg, &id, &client, &cache, supervisor.as_ref()) => {}
        () = shutdown_signal() => tracing::info!("señal recibida; parando"),
    }
    // Para las tareas y mata llama-server (kill_on_drop) antes de salir.
    tasks.shutdown().await;
    Ok(())
}

/// `rpc-server` escuchando en la IP con la que el Mac llega al teléfono
/// (nunca en todas las interfaces: no tiene autenticación, D-56).
fn rpc_proc(cfg: &Config, advertise_url: &str) -> Option<supervisor::Proc> {
    if cfg.rpc_command.is_empty() {
        return None;
    }
    let Some(host) = Url::parse(advertise_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_matches(['[', ']']).to_string()))
    else {
        tracing::warn!(%advertise_url, "sin IP propia: el modo dividido queda desactivado");
        return None;
    };
    let mut command = cfg.rpc_command.clone();
    command.extend(["-H".into(), host, "-p".into(), cfg.rpc_port.to_string()]);
    let cache = cfg.state_dir.join("cache");
    Some(supervisor::Proc {
        command,
        log: cfg.state_dir.join("rpc-server.log"),
        // La caché de tensores evita reenviar el modelo en cada arranque.
        env: vec![("LLAMA_CACHE".into(), cache.to_string_lossy().into_owned())],
    })
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// En `--mock` el id se deriva del nombre para poder correr varios agentes en
/// la misma máquina; si no, se genera una vez y se guarda en disco.
fn node_id(cfg: &Config) -> anyhow::Result<NodeId> {
    if cfg.mock.is_some() {
        let seed = format!("mock/{}", cfg.name);
        return Ok(Uuid::new_v5(&Uuid::NAMESPACE_OID, seed.as_bytes()));
    }
    load_or_create_node_id(&cfg.state_dir)
}

pub fn load_or_create_node_id(dir: &Path) -> anyhow::Result<NodeId> {
    let path = dir.join("node_id");
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            return Uuid::parse_str(text.trim())
                .with_context(|| format!("{} no contiene un UUID", path.display()));
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("leyendo {}", path.display()));
        }
        Err(_) => {}
    }
    let id = Uuid::new_v4();
    std::fs::create_dir_all(dir).with_context(|| format!("creando {}", dir.display()))?;
    std::fs::write(&path, format!("{id}\n"))
        .with_context(|| format!("escribiendo {}", path.display()))?;
    tracing::info!(%id, path = %path.display(), "node_id nuevo generado");
    Ok(id)
}

fn advertise_url(cfg: &Config) -> String {
    if let Some(url) = &cfg.advertise_url {
        return url.clone();
    }
    if cfg.mock.is_some() {
        return cfg.llama_url.clone();
    }
    match detect_advertise_url(&cfg.coordinator_url, &cfg.llama_url) {
        Some(url) => {
            tracing::warn!(%url, "advertise_url no definido; usando la IP de la interfaz que llega al coordinador");
            url
        }
        None => {
            tracing::warn!(url = %cfg.llama_url, "advertise_url no definido y no se pudo detectar la IP; el Mac probablemente no llegará a llama-server");
            cfg.llama_url.clone()
        }
    }
}

/// IP local de la interfaz con ruta hacia el coordinador (la Wi-Fi en el
/// teléfono) + puerto de `llama_url`. `connect` en UDP no envía paquetes.
fn detect_advertise_url(coordinator_url: &str, llama_url: &str) -> Option<String> {
    let coord = Url::parse(coordinator_url).ok()?;
    let target = (coord.host_str()?, coord.port_or_known_default()?)
        .to_socket_addrs()
        .ok()?
        .next()?;
    let bind = if target.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect(target).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    let port = Url::parse(llama_url).ok()?.port_or_known_default()?;
    let host = match ip {
        std::net::IpAddr::V6(v6) => format!("[{v6}]"),
        v4 => v4.to_string(),
    };
    Some(format!("http://{host}:{port}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_persists() {
        let dir = std::env::temp_dir().join(format!("pc-agent-id-{}", Uuid::new_v4()));
        let a = load_or_create_node_id(&dir).unwrap();
        let b = load_or_create_node_id(&dir).unwrap();
        assert_eq!(a, b);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mock_ids_depend_on_name() {
        let mut a = Config::new("mock-1", "http://127.0.0.1:7800");
        a.mock = Some(config::MockMetrics {
            temp_c: 36.0,
            battery_pct: 80,
            charging: true,
        });
        let mut b = a.clone();
        b.name = "mock-2".into();
        assert_ne!(node_id(&a).unwrap(), node_id(&b).unwrap());
        assert_eq!(node_id(&a).unwrap(), node_id(&a.clone()).unwrap());
    }

    #[test]
    fn rpc_server_binds_to_the_advertised_ip() {
        let mut cfg = Config::new("n", "http://127.0.0.1:7800");
        assert!(rpc_proc(&cfg, "http://192.168.1.9:8080").is_none());
        cfg.rpc_command = vec!["rpc-server".into(), "-c".into()];
        cfg.rpc_port = 50060;
        let p = rpc_proc(&cfg, "http://192.168.1.9:8080").unwrap();
        assert_eq!(
            p.command,
            vec!["rpc-server", "-c", "-H", "192.168.1.9", "-p", "50060"]
        );
        assert_eq!(p.env[0].0, "LLAMA_CACHE");
    }

    #[test]
    fn detects_loopback_route() {
        let url = detect_advertise_url("http://127.0.0.1:7800", "http://127.0.0.1:8080").unwrap();
        assert_eq!(url, "http://127.0.0.1:8080");
    }
}
