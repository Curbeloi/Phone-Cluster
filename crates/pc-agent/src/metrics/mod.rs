//! Recolección de métricas en ciclos independientes; cada ciclo guarda su
//! último valor en una caché que el heartbeat lee sin esperar a nadie.

pub mod battery;
pub mod llama;
pub mod memory;
pub mod thermal;

use std::sync::Arc;
use std::time::Duration;

use pc_proto::{Battery, LlamaInfo, LlamaStatus, Memory, Thermal};
use tokio::sync::RwLock;
use tokio::task::JoinSet;

use crate::config::Config;

/// Último valor conocido de cada métrica.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub battery: Option<Battery>,
    pub thermal: Option<Thermal>,
    pub memory: Option<Memory>,
    /// `None` hasta la primera consulta a `/health`.
    pub llama_status: Option<LlamaStatus>,
    pub props: Option<llama::Props>,
    pub slots: Option<llama::Slots>,
}

impl Snapshot {
    pub fn llama_info(&self) -> LlamaInfo {
        let props = self.props.clone().unwrap_or_default();
        LlamaInfo {
            status: self.llama_status.unwrap_or(LlamaStatus::Down),
            model: props.model,
            ctx_size: props.ctx_size,
            slots_total: self.slots.map(|s| s.total).or(props.slots_total),
            slots_busy: self.slots.map(|s| s.busy),
        }
    }
}

pub type Cache = Arc<RwLock<Snapshot>>;

const FAST: Duration = Duration::from_secs(5);
const BATTERY: Duration = Duration::from_secs(30);
const PROPS: Duration = Duration::from_secs(60);

/// Una métrica: cómo leerla y dónde guardarla.
#[derive(Debug, Clone, Copy)]
enum Metric {
    Battery,
    Thermal,
    Memory,
    Health,
    Slots,
    Props,
}

impl Metric {
    const ALL: [Metric; 6] = [
        Metric::Battery,
        Metric::Thermal,
        Metric::Memory,
        Metric::Health,
        Metric::Slots,
        Metric::Props,
    ];

    fn interval(self) -> Duration {
        match self {
            Metric::Battery => BATTERY,
            Metric::Props => PROPS,
            _ => FAST,
        }
    }
}

/// Lee una métrica y la guarda en la caché.
async fn collect(metric: Metric, cfg: &Config, client: &reqwest::Client, cache: &Cache) {
    let base = cfg.llama_url.as_str();
    match metric {
        Metric::Battery => {
            let v = match &cfg.mock {
                Some(m) => Some(Battery {
                    pct: m.battery_pct,
                    charging: m.charging,
                    temp_c: Some(m.temp_c),
                }),
                None => battery::read(cfg.battery_file.as_deref()).await,
            };
            cache.write().await.battery = v;
        }
        Metric::Thermal => {
            let v = match &cfg.mock {
                // Bajo carga el SoC va bastante más caliente que la batería.
                Some(m) => Some(Thermal {
                    max_c: m.temp_c + 10.0,
                }),
                None => thermal::read(&cfg.thermal_zones).await,
            };
            cache.write().await.thermal = v;
        }
        Metric::Memory => {
            let v = match &cfg.mock {
                Some(_) => Some(Memory {
                    total_mb: 8192,
                    available_mb: 4096,
                }),
                None => memory::read(),
            };
            cache.write().await.memory = v;
        }
        Metric::Health => {
            let v = llama::health(client, base).await;
            cache.write().await.llama_status = Some(v);
        }
        Metric::Slots => {
            let v = llama::slots(client, base).await;
            cache.write().await.slots = v;
        }
        Metric::Props => {
            // Si falla se conserva el último valor.
            if let Some(v) = llama::props(client, base).await {
                cache.write().await.props = Some(v);
            }
        }
    }
}

/// Ejecuta un comando con timeout y devuelve su stdout si terminó bien.
pub(crate) async fn command_output(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Option<String> {
    let child = tokio::process::Command::new(program)
        .args(args)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .output();
    match tokio::time::timeout(timeout, child).await {
        Ok(Ok(out)) if out.status.success() => {
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        Ok(Ok(out)) => {
            tracing::debug!(program, status = %out.status, "el comando falló");
            None
        }
        Ok(Err(e)) => {
            tracing::debug!(program, error = %e, "comando no disponible");
            None
        }
        Err(_) => {
            tracing::warn!(
                program,
                secs = timeout.as_secs(),
                "el comando no respondió a tiempo"
            );
            None
        }
    }
}

/// Recolecta todas las métricas una vez, en paralelo.
pub async fn collect_once(cfg: &Config, client: &reqwest::Client, cache: &Cache) {
    let futures = Metric::ALL.map(|m| collect(m, cfg, client, cache));
    futures::future::join_all(futures).await;
}

/// Lanza un ciclo por métrica, que espera su intervalo antes de cada lectura
/// (la primera la hace `collect_once`). Las tareas mueren al soltar el `JoinSet`.
pub fn spawn_all(cfg: Arc<Config>, client: reqwest::Client, cache: Cache, tasks: &mut JoinSet<()>) {
    for metric in Metric::ALL {
        let (cfg, client, cache) = (cfg.clone(), client.clone(), cache.clone());
        tasks.spawn(async move {
            loop {
                let mut wait = metric.interval();
                // Hasta conseguir /props por primera vez, reintentar al ritmo rápido.
                if matches!(metric, Metric::Props) && cache.read().await.props.is_none() {
                    wait = FAST;
                }
                tokio::time::sleep(wait).await;
                collect(metric, &cfg, &client, &cache).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llama_info_defaults_to_down_and_prefers_live_slots() {
        let mut s = Snapshot::default();
        let info = s.llama_info();
        assert_eq!(info.status, LlamaStatus::Down);
        assert_eq!(info.slots_total, None);

        s.llama_status = Some(LlamaStatus::Ok);
        s.props = Some(llama::Props {
            model: Some("m".into()),
            ctx_size: Some(4096),
            slots_total: Some(4),
        });
        assert_eq!(s.llama_info().slots_total, Some(4));
        assert_eq!(s.llama_info().slots_busy, None);

        s.slots = Some(llama::Slots { total: 2, busy: 1 });
        let info = s.llama_info();
        assert_eq!((info.slots_total, info.slots_busy), (Some(2), Some(1)));
        assert_eq!(info.model.as_deref(), Some("m"));
    }
}
