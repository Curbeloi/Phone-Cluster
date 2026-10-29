//! `pc-coordinator`: registro de nodos, política de selección, proxy OpenAI
//! y dashboard. Todo el estado vive en memoria.

pub mod api;
pub mod config;
pub mod devices;
pub mod events;
pub mod join;
pub mod policy;
pub mod proxy;
pub mod registry;
pub mod split;

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::net::TcpListener;

use crate::api::AppState;
use crate::config::Config;
use crate::devices::Devices;
use crate::join::Joins;
use crate::registry::Registry;
use crate::split::Split;

const TICK: Duration = Duration::from_secs(2);

/// Sirve la API (`api`) y el dashboard (`dashboard`) hasta que falle uno.
pub async fn serve(cfg: Config, api: TcpListener, dashboard: TcpListener) -> anyhow::Result<()> {
    cfg.validate()?;
    let api_addr = api.local_addr()?;
    let dash_addr = dashboard.local_addr()?;
    if !dash_addr.ip().is_loopback() {
        tracing::warn!(%dash_addr, "el dashboard no está en loopback y permite administrar sin token");
    }
    let registry = Arc::new(Registry::new(cfg.thresholds()));
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .build()
        .context("creando cliente HTTP")?;
    let lan_url = lan_base_url(api_addr);
    if api_addr.ip().is_loopback() {
        tracing::warn!(%api_addr, "la API escucha solo en loopback: los teléfonos no podrán conectarse");
    }
    let devices = Devices::new(
        cfg.adb.clone(),
        lan_url.clone(),
        api_addr.port(),
        cfg.token.clone(),
        registry.events().clone(),
    );
    devices.spawn();
    if cfg.advertise_mdns && !api_addr.ip().is_loopback() {
        advertise(api_addr.port());
    }
    let joins = Arc::new(Joins::new(registry.events().clone()));
    let cfg = Arc::new(cfg);
    let split = Split::new(registry.clone(), cfg.clone(), client.clone());
    split.spawn();
    let base = AppState {
        registry: registry.clone(),
        cfg,
        client,
        split,
        trusted: false,
        api_addr,
        devices,
        joins: joins.clone(),
    };
    let dash_state = AppState {
        trusted: true,
        ..base.clone()
    };

    let ticker = registry.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK);
        loop {
            interval.tick().await;
            ticker.tick(Instant::now(), api::now_ms());
            joins.prune(Instant::now());
        }
    });

    tracing::info!(api = %api_addr, dashboard = %dash_addr, openai = %format!("{lan_url}/v1"), "pc-coordinator escuchando");
    // ConnectInfo: el alta guarda la IP del teléfono que la pide.
    let api_server = axum::serve(
        api,
        api::router(base).into_make_service_with_connect_info::<SocketAddr>(),
    );
    let dash_server = axum::serve(
        dashboard,
        api::router(dash_state).into_make_service_with_connect_info::<SocketAddr>(),
    );
    tokio::try_join!(async { api_server.await.context("servidor API") }, async {
        dash_server.await.context("servidor del dashboard")
    },)?;
    Ok(())
}

/// Anuncia el coordinador con el Bonjour del sistema (`dns-sd -R`), para que
/// la app Android lo encuentre sin escribir ninguna IP.
fn advertise(port: u16) {
    let args = [
        "-R",
        "Phone Cluster",
        "_phonecluster._tcp",
        "local",
        &port.to_string(),
        "v=1",
    ]
    .map(String::from);
    let child = watched("dns-sd", &args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match child {
        Ok(mut child) => {
            tracing::info!(port, "anunciado en la red como _phonecluster._tcp");
            tokio::spawn(async move {
                let status = child.wait().await;
                tracing::warn!(
                    ?status,
                    "dns-sd terminó: la app Android ya no encontrará el Mac sola"
                );
            });
        }
        Err(e) => tracing::warn!(
            error = %e,
            "sin dns-sd: la app Android tendrá que usar la IP del Mac"
        ),
    }
}

/// `program args` envuelto en un `sh` que lo mata cuando muere el
/// coordinador. Si el coordinador termina sin ejecutar destructores (SIGKILL,
/// cerrar la app de Mac), el `sh` lo nota por `$PPID` en 2 s.
///
/// Sin `kill_on_drop`: un SIGKILL al `sh` no ejecuta su `trap` y dejaría el
/// proceso huérfano. Para pararlo antes, [`terminate`].
pub(crate) fn watched(program: &str, args: &[String]) -> tokio::process::Command {
    const SCRIPT: &str = r#""$@" &
c=$!
trap 'kill $c 2>/dev/null' EXIT INT TERM
while kill -0 "$PPID" 2>/dev/null && kill -0 "$c" 2>/dev/null; do sleep 2; done"#;
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.args(["-c", SCRIPT, "sh", program])
        .args(args)
        .stdin(std::process::Stdio::null());
    cmd
}

/// SIGTERM a un proceso de [`watched`]: su `trap` mata también al de dentro.
pub(crate) fn terminate(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
}

/// `http://<IP>:<puerto>` con el que la LAN llega al coordinador. Si se escucha
/// en todas las interfaces, se usa la IP con ruta por defecto (UDP `connect`
/// no envía nada). Se recalcula en cada llamada: la IP del Mac puede cambiar.
pub fn lan_base_url(addr: SocketAddr) -> String {
    let ip = if addr.ip().is_unspecified() {
        UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| {
                s.connect("192.0.2.1:9")?;
                s.local_addr()
            })
            .map(|a| a.ip())
            .unwrap_or(IpAddr::from([127, 0, 0, 1]))
    } else {
        addr.ip()
    };
    format!("http://{}", SocketAddr::new(ip, addr.port()))
}
