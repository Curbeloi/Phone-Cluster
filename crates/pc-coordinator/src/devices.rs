//! Teléfonos por ADB inalámbrico: los descubre por mDNS, los vincula con el
//! código que muestra el teléfono y les instala y arranca `pc-agent` +
//! `llama-server` sin tocar el teléfono (ver DECISIONS.md, D-32 y siguientes).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use tokio::process::Command;

use crate::config::{AdbConfig, Secret};
use crate::events::{Event, EventBus};

const SCAN_EVERY: Duration = Duration::from_secs(3);
const CHECK_EVERY: Duration = Duration::from_secs(10);
const CONNECT_EVERY: Duration = Duration::from_secs(10);
const SHORT: Duration = Duration::from_secs(15);
const PUSH: Duration = Duration::from_secs(30 * 60);
const AGENT: &str = "pc-agent";
const LLAMA: &str = "llama-server";
const APP_PACKAGE: &str = "dev.phonecluster.app";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// El teléfono muestra "Vincular con código": falta el código.
    Pairable,
    /// Visible, pero ADB aún no está conectado.
    Connecting,
    /// Conectado por USB sin aceptar la huella en el teléfono.
    Unauthorized,
    Installing,
    Running,
    /// Tiene la app Android: se une por ella y ADB no instala nada.
    App,
    Error,
}

/// Lo que el dashboard muestra de cada teléfono encontrado.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeviceView {
    /// IP del teléfono, o el serial de ADB si no tiene IP (USB, emulador).
    pub key: String,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub phase: Phase,
    pub detail: Option<String>,
    /// `ip:puerto` del servicio de vinculación, mientras el teléfono lo anuncia.
    pub pair_address: Option<String>,
    /// Nombre con el que aparece como nodo.
    pub node_name: Option<String>,
}

impl DeviceView {
    fn new(key: &str, phase: Phase) -> Self {
        Self {
            key: key.to_string(),
            model: None,
            serial: None,
            phase,
            detail: None,
            pair_address: None,
            node_name: None,
        }
    }
}

/// Servicio anunciado por un teléfono con la depuración inalámbrica activa.
#[derive(Debug, Clone, PartialEq)]
pub struct MdnsService {
    pub instance: String,
    pub pairing: bool,
    pub address: String,
}

impl MdnsService {
    fn ip(&self) -> &str {
        host_of(&self.address)
    }
}

/// Interpreta `adb mdns services`.
pub fn parse_mdns(text: &str) -> Vec<MdnsService> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let (instance, kind, address) = (parts.next()?, parts.next()?, parts.next()?);
            let pairing = match kind.trim_end_matches('.') {
                "_adb-tls-pairing._tcp" => true,
                "_adb-tls-connect._tcp" => false,
                _ => return None,
            };
            address.contains(':').then(|| MdnsService {
                instance: instance.to_string(),
                pairing,
                address: address.to_string(),
            })
        })
        .collect()
}

/// Interpreta `adb devices`: `(serial, estado)`.
pub fn parse_devices(text: &str) -> Vec<(String, String)> {
    text.lines()
        .skip_while(|l| !l.starts_with("List of devices"))
        .skip(1)
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            Some((parts.next()?.to_string(), parts.next()?.to_string()))
        })
        .collect()
}

fn host_of(address: &str) -> &str {
    address.rsplit_once(':').map_or(address, |(h, _)| h)
}

/// Clave estable de un dispositivo conectado: su IP si se conoce.
fn device_key(serial: &str, services: &[MdnsService]) -> String {
    if serial.contains(':') && !serial.starts_with("adb-") {
        return host_of(serial).to_string();
    }
    // Serial mDNS: "adb-XXXX-yyyy._adb-tls-connect._tcp"
    let instance = serial.split("._adb-tls-connect").next().unwrap_or(serial);
    services
        .iter()
        .find(|s| !s.pairing && s.instance == instance)
        .map_or_else(|| serial.to_string(), |s| s.ip().to_string())
}

/// `"Pixel 7"` + `"192.168.1.21"` → `"pixel-7-21"`.
pub fn node_name(model: &str, key: &str) -> String {
    let base: String = model
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let base = base
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let suffix = key.rsplit(['.', '-', ':']).next().unwrap_or(key);
    let base = if base.is_empty() {
        "telefono".into()
    } else {
        base
    };
    format!("{base}-{suffix}")
}

/// FNV-1a: huella estable entre ejecuciones, sin dependencias.
fn fingerprint(parts: &[String]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in parts.join("\n").bytes() {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Ficheros a instalar, comprobados en disco.
#[derive(Debug, Clone)]
pub struct Artifacts {
    pub agent: PathBuf,
    pub llama: PathBuf,
    pub model: PathBuf,
}

impl Artifacts {
    pub fn find(dir: &Path) -> Result<Self, String> {
        let agent = dir.join(AGENT);
        let llama = dir.join(LLAMA);
        let model = find_model(dir);
        let mut missing = vec![];
        if !agent.is_file() {
            missing.push(AGENT.to_string());
        }
        if !llama.is_file() {
            missing.push(LLAMA.to_string());
        }
        let Some(model) = model else {
            missing.push("modelo .gguf".into());
            return Err(missing.join(", "));
        };
        if !missing.is_empty() {
            return Err(missing.join(", "));
        }
        Ok(Self {
            agent,
            llama,
            model,
        })
    }

    fn model_file(&self) -> String {
        file_name(&self.model)
    }

    fn stamp(&self) -> Vec<String> {
        [&self.agent, &self.llama, &self.model]
            .iter()
            .map(|p| {
                let meta = std::fs::metadata(p).ok();
                let size = meta.as_ref().map_or(0, std::fs::Metadata::len);
                let mtime = meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());
                format!("{}:{size}:{mtime}", file_name(p))
            })
            .collect()
    }
}

/// Primer `.gguf` de `dir`: el modelo que se instala en los teléfonos.
pub fn find_model(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "gguf"))
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `config.toml` del agente en el teléfono.
#[derive(Serialize)]
struct AgentFile {
    name: String,
    coordinator_url: String,
    token: String,
    advertise_url: String,
    llama_url: String,
    heartbeat_secs: u64,
    llama_command: Vec<String>,
}

const START_SH: &str = r#"#!/system/bin/sh
# Arranca pc-agent desacoplado de la sesión de adb; el agente lanza llama-server.
cd "$(dirname "$0")" || exit 1
export HOME="$PWD"
if command -v setsid >/dev/null 2>&1; then
  setsid nohup ./pc-agent --config ./config.toml > agent.log 2>&1 < /dev/null &
else
  nohup ./pc-agent --config ./config.toml > agent.log 2>&1 < /dev/null &
fi
"#;

#[derive(Default)]
struct State {
    views: HashMap<String, DeviceView>,
    /// Dispositivos con una instalación o comprobación en curso.
    busy: HashSet<String>,
    last_check: HashMap<String, Instant>,
    last_connect: HashMap<String, Instant>,
    /// Instalación forzada pedida desde el dashboard.
    force: HashSet<String>,
}

pub struct Devices {
    adb: Option<PathBuf>,
    cfg: AdbConfig,
    /// URL con la que los teléfonos llegan al coordinador.
    coordinator_url: String,
    coordinator_port: u16,
    token: Secret,
    events: EventBus,
    state: Mutex<State>,
}

/// Estado de ADB y de los ficheros, para el dashboard.
#[derive(Debug, Serialize)]
pub struct Status {
    pub enabled: bool,
    pub adb: Option<String>,
    pub artifacts_missing: Option<String>,
    pub model: Option<String>,
    pub devices: Vec<DeviceView>,
}

impl Devices {
    pub fn new(
        cfg: AdbConfig,
        coordinator_url: String,
        coordinator_port: u16,
        token: Secret,
        events: EventBus,
    ) -> Arc<Self> {
        let adb = if cfg.enabled {
            find_adb(cfg.adb_path.as_deref())
        } else {
            None
        };
        if cfg.enabled && adb.is_none() {
            tracing::warn!(
                "adb no encontrado: instala Android platform-tools o define [adb] adb_path"
            );
        }
        Arc::new(Self {
            adb,
            cfg,
            coordinator_url,
            coordinator_port,
            token,
            events,
            state: Mutex::default(),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn status(&self) -> Status {
        let mut devices: Vec<DeviceView> = self.state().views.values().cloned().collect();
        devices.sort_by(|a, b| a.key.cmp(&b.key));
        let artifacts = Artifacts::find(&self.cfg.artifacts_dir);
        Status {
            enabled: self.cfg.enabled,
            adb: self.adb.as_ref().map(|p| p.display().to_string()),
            artifacts_missing: artifacts.as_ref().err().cloned(),
            model: artifacts.ok().map(|a| a.model_file()),
            devices,
        }
    }

    /// Bucle de descubrimiento. No hace nada si ADB está desactivado.
    pub fn spawn(self: &Arc<Self>) {
        if self.adb.is_none() {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            // Un servidor adb que lleva tiempo arrancado (Android Studio, el
            // emulador) puede tener el descubrimiento mDNS atascado y no ver
            // teléfonos que macOS sí ve: se reinicia una vez al arrancar.
            let _ = this.adb(&["kill-server"], SHORT).await;
            if let Err(e) = this.adb(&["start-server"], SHORT).await {
                tracing::warn!(error = %e, "no se pudo arrancar el servidor de adb");
            }
            tracing::info!(adb = ?this.adb, "buscando teléfonos con depuración inalámbrica");
            loop {
                this.scan().await;
                tokio::time::sleep(SCAN_EVERY).await;
            }
        });
    }

    async fn adb(&self, args: &[&str], timeout: Duration) -> Result<String, String> {
        let Some(adb) = &self.adb else {
            return Err("adb no encontrado".into());
        };
        let child = Command::new(adb)
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(timeout, child).await {
            Err(_) => Err(format!("adb {} no respondió", args.first().unwrap_or(&""))),
            Ok(Err(e)) => Err(e.to_string()),
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
                if out.status.success() {
                    Ok(stdout + &stderr)
                } else {
                    Err(format!("{stdout}{stderr}").trim().to_string())
                }
            }
        }
    }

    async fn shell(&self, serial: &str, script: &str, timeout: Duration) -> Result<String, String> {
        self.adb(&["-s", serial, "shell", script], timeout).await
    }

    fn publish(&self, view: DeviceView) {
        let mut st = self.state();
        if st.views.get(&view.key) != Some(&view) {
            st.views.insert(view.key.clone(), view.clone());
            self.events.publish(Event::DeviceUpdated(Box::new(view)));
        }
    }

    fn update(&self, key: &str, f: impl FnOnce(&mut DeviceView)) {
        let current = self.state().views.get(key).cloned();
        if let Some(mut v) = current {
            f(&mut v);
            self.publish(v);
        }
    }

    async fn scan(self: &Arc<Self>) {
        let services = match self.adb(&["mdns", "services"], SHORT).await {
            Ok(out) => parse_mdns(&out),
            Err(e) => {
                tracing::debug!(error = %e, "adb mdns services falló");
                vec![]
            }
        };
        let devices = match self.adb(&["devices"], SHORT).await {
            Ok(out) => parse_devices(&out),
            Err(e) => {
                tracing::warn!(error = %e, "adb devices falló");
                return;
            }
        };

        let mut seen = HashSet::new();
        let mut connected_ips = HashSet::new();
        for (serial, state) in &devices {
            let key = device_key(serial, &services);
            seen.insert(key.clone());
            if state != "device" {
                let phase = if state == "unauthorized" {
                    Phase::Unauthorized
                } else {
                    Phase::Connecting
                };
                let mut v = self
                    .state()
                    .views
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| DeviceView::new(&key, phase));
                v.phase = phase;
                v.serial = Some(serial.clone());
                v.detail = Some(match phase {
                    Phase::Unauthorized => "Acepta la depuración en el teléfono".into(),
                    _ => format!("ADB: {state}"),
                });
                self.publish(v);
                continue;
            }
            connected_ips.insert(key.clone());
            self.maybe_check(serial, &key);
        }

        let now = Instant::now();
        for svc in &services {
            let ip = svc.ip().to_string();
            if connected_ips.contains(&ip) {
                continue;
            }
            seen.insert(ip.clone());
            if svc.pairing {
                let mut v = self
                    .state()
                    .views
                    .get(&ip)
                    .cloned()
                    .unwrap_or_else(|| DeviceView::new(&ip, Phase::Pairable));
                v.phase = Phase::Pairable;
                v.pair_address = Some(svc.address.clone());
                v.detail = Some("Escribe el código de 6 dígitos que muestra el teléfono".into());
                self.publish(v);
                continue;
            }
            // Servicio de conexión de un teléfono no conectado: intentar.
            let due = {
                let mut st = self.state();
                let due = st
                    .last_connect
                    .get(&ip)
                    .is_none_or(|t| now.duration_since(*t) >= CONNECT_EVERY);
                if due {
                    st.last_connect.insert(ip.clone(), now);
                }
                due
            };
            let has_pairing = services.iter().any(|s| s.pairing && s.ip() == ip);
            if !due || has_pairing {
                continue;
            }
            let mut v = self
                .state()
                .views
                .get(&ip)
                .cloned()
                .unwrap_or_else(|| DeviceView::new(&ip, Phase::Connecting));
            match self.adb(&["connect", &svc.address], SHORT).await {
                Ok(out) if out.contains("connected to") => {
                    v.phase = Phase::Connecting;
                    v.detail = Some("Conectado; preparando…".into());
                }
                Ok(out) | Err(out) => {
                    tracing::debug!(%ip, out = %out.trim(), "adb connect no lo logró");
                    v.phase = Phase::Connecting;
                    v.detail = Some(
                        "Sin vincular: en el teléfono pulsa «Vincular dispositivo con código de vinculación»"
                            .into(),
                    );
                }
            }
            self.publish(v);
        }

        let gone: Vec<String> = {
            let st = self.state();
            st.views
                .keys()
                .filter(|k| !seen.contains(*k) && !st.busy.contains(*k))
                .cloned()
                .collect()
        };
        for key in gone {
            self.state().views.remove(&key);
            self.events.publish(Event::DeviceRemoved(key));
        }
    }

    /// Lanza una comprobación (y, si hace falta, una instalación) en segundo plano.
    fn maybe_check(self: &Arc<Self>, serial: &str, key: &str) {
        {
            let mut st = self.state();
            if st.busy.contains(key) {
                return;
            }
            let forced = st.force.contains(key);
            let due = st
                .last_check
                .get(key)
                .is_none_or(|t| t.elapsed() >= CHECK_EVERY);
            let known = st
                .views
                .get(key)
                .is_some_and(|v| matches!(v.phase, Phase::Running | Phase::App));
            if known && !due && !forced {
                return;
            }
            if !forced && !due && st.views.get(key).is_some_and(|v| v.phase == Phase::Error) {
                return;
            }
            st.busy.insert(key.to_string());
            st.last_check.insert(key.to_string(), Instant::now());
            if !st.views.contains_key(key) {
                let mut v = DeviceView::new(key, Phase::Connecting);
                v.serial = Some(serial.to_string());
                st.views.insert(key.to_string(), v);
            }
        }
        let (this, serial, key) = (self.clone(), serial.to_string(), key.to_string());
        tokio::spawn(async move {
            let forced = this.state().force.remove(&key);
            if let Err(e) = this.ensure(&serial, &key, forced).await {
                tracing::warn!(device = %key, error = %e, "instalación fallida");
                this.update(&key, |v| {
                    v.phase = Phase::Error;
                    v.detail = Some(e);
                });
            }
            this.state().busy.remove(&key);
        });
    }

    /// Fuerza reinstalar y reiniciar en el próximo escaneo.
    pub fn reinstall(&self, key: &str) -> bool {
        let mut st = self.state();
        if !st.views.contains_key(key) {
            return false;
        }
        st.force.insert(key.to_string());
        st.last_check.remove(key);
        true
    }

    pub async fn pair(&self, key: &str, code: &str) -> Result<String, String> {
        let code = code.trim();
        if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
            return Err("el código tiene 6 dígitos".into());
        }
        let address = self
            .state()
            .views
            .get(key)
            .and_then(|v| v.pair_address.clone())
            .ok_or(
                "ese teléfono ya no ofrece vinculación; vuelve a abrir el diálogo en el teléfono",
            )?;
        let out = self.adb(&["pair", &address, code], SHORT).await?;
        if out.contains("Successfully paired") {
            tracing::info!(device = %key, "teléfono vinculado");
            self.update(key, |v| {
                v.phase = Phase::Connecting;
                v.pair_address = None;
                v.detail = Some("Vinculado; conectando…".into());
            });
            self.state().last_connect.remove(key);
            Ok("vinculado".into())
        } else {
            Err(out.trim().to_string())
        }
    }

    async fn ensure(&self, serial: &str, key: &str, forced: bool) -> Result<(), String> {
        let artifacts = Artifacts::find(&self.cfg.artifacts_dir).map_err(|m| {
            format!(
                "Faltan ficheros en {}: {m}. Ejecuta tools/build-android.sh",
                self.cfg.artifacts_dir.display()
            )
        })?;
        let dir = self.cfg.remote_dir.trim_end_matches('/').to_string();
        let probe = format!(
            "getprop ro.product.model; echo '@@'; getprop ro.product.cpu.abi; echo '@@'; \
             cat {dir}/.deployed 2>/dev/null; echo '@@'; pidof {AGENT}; echo '@@'; \
             ip -f inet addr show wlan0 2>/dev/null | grep -o 'inet [0-9.]*'; echo '@@'; \
             pm list packages {APP_PACKAGE} 2>/dev/null"
        );
        let out = self.shell(serial, &probe, SHORT).await?;
        let parts: Vec<&str> = out.split("@@").map(str::trim).collect();
        let field = |i: usize| parts.get(i).copied().unwrap_or_default();
        let model = field(0).to_string();
        let abi = field(1);
        let deployed = field(2);
        let running = !field(3).is_empty();
        let wlan_ip = field(4).trim_start_matches("inet ").trim().to_string();
        let has_app = field(5)
            .lines()
            .any(|l| l.trim() == format!("package:{APP_PACKAGE}"));

        let name = node_name(&model, key);
        self.update(key, |v| {
            v.model = Some(model.clone()).filter(|m| !m.is_empty());
            v.serial = Some(serial.to_string());
            v.node_name = Some(name.clone());
        });
        if abi != "arm64-v8a" {
            return Err(format!("ABI «{abi}» no soportada (solo arm64-v8a)"));
        }
        if has_app && !forced {
            // Con la app y el agente de ADB a la vez, el teléfono saldría dos
            // veces y cargaría el modelo dos veces (D-58).
            if running {
                tracing::info!(device = %key, "tiene la app: se para el agente instalado por ADB");
                self.shell(
                    serial,
                    &format!("pkill -x {AGENT}; pkill -x {LLAMA}; true"),
                    SHORT,
                )
                .await?;
            }
            self.update(key, |v| {
                v.phase = Phase::App;
                v.detail = Some("Usa la app Phone Cluster; ADB no instala nada.".into());
            });
            return Ok(());
        }

        let (coordinator_url, advertise_url) = self.urls(serial, key, &wlan_ip).await?;
        let alias = self.cfg.alias_for(&artifacts.model_file());
        let port = self.cfg.llama_port.to_string();
        let mut llama_command = vec![
            format!("{dir}/{LLAMA}"),
            "-m".into(),
            format!("{dir}/{}", artifacts.model_file()),
            "--alias".into(),
            alias,
            "--host".into(),
            "0.0.0.0".into(),
            "--port".into(),
            port.clone(),
        ];
        llama_command.extend(self.cfg.llama_args.iter().cloned());
        let agent_file = AgentFile {
            name: name.clone(),
            coordinator_url,
            token: self.token.expose().to_string(),
            advertise_url,
            llama_url: format!("http://127.0.0.1:{port}"),
            heartbeat_secs: 5,
            llama_command,
        };
        let config = toml::to_string(&agent_file).map_err(|e| e.to_string())?;
        let mut stamp = artifacts.stamp();
        stamp.push(config.clone());
        stamp.push(START_SH.to_string());
        let fp = fingerprint(&stamp);

        if deployed == fp && running && !forced {
            self.update(key, |v| {
                v.phase = Phase::Running;
                v.detail = None;
            });
            return Ok(());
        }
        if deployed == fp && !forced {
            self.set_phase(key, Phase::Installing, "Arrancando el agente…");
            self.start(serial, &dir).await?;
            self.finish(serial, key, &dir, &fp).await?;
            return Ok(());
        }

        tracing::info!(device = %key, %name, "instalando");
        self.shell(serial, &format!("mkdir -p {dir}"), SHORT)
            .await?;
        for (label, path) in [
            ("pc-agent", &artifacts.agent),
            ("llama-server", &artifacts.llama),
        ] {
            self.set_phase(key, Phase::Installing, &format!("Copiando {label}…"));
            self.push(serial, path, &format!("{dir}/{}", file_name(path)))
                .await?;
        }
        let model_remote = format!("{dir}/{}", artifacts.model_file());
        let local_size = std::fs::metadata(&artifacts.model)
            .map(|m| m.len())
            .unwrap_or(0);
        let remote_size = self
            .shell(
                serial,
                &format!("stat -c %s {model_remote} 2>/dev/null"),
                SHORT,
            )
            .await
            .unwrap_or_default();
        if remote_size.trim().parse::<u64>().ok() != Some(local_size) {
            // Un modelo anterior con otro nombre se borra para no llenar el teléfono.
            self.shell(serial, &format!("rm -f {dir}/*.gguf"), SHORT)
                .await?;
            let mb = local_size / (1024 * 1024);
            self.set_phase(
                key,
                Phase::Installing,
                &format!("Copiando el modelo ({mb} MB), puede tardar unos minutos…"),
            );
            self.push(serial, &artifacts.model, &model_remote).await?;
        }
        self.set_phase(key, Phase::Installing, "Configurando…");
        self.push_text(serial, &config, &format!("{dir}/config.toml"))
            .await?;
        self.push_text(serial, START_SH, &format!("{dir}/start.sh"))
            .await?;
        self.shell(
            serial,
            &format!(
                // config.toml lleva el token: solo para el usuario shell.
                "chmod 700 {dir} && chmod 755 {dir}/{AGENT} {dir}/{LLAMA} {dir}/start.sh && chmod 600 {dir}/config.toml"
            ),
            SHORT,
        )
        .await?;
        self.start(serial, &dir).await?;
        self.finish(serial, key, &dir, &fp).await
    }

    fn set_phase(&self, key: &str, phase: Phase, detail: &str) {
        self.update(key, |v| {
            v.phase = phase;
            v.detail = Some(detail.to_string());
        });
    }

    /// URL del coordinador vista desde el teléfono y URL con la que el Mac llega
    /// a su llama-server. En el emulador se usa 10.0.2.2 y `adb forward`.
    async fn urls(
        &self,
        serial: &str,
        key: &str,
        wlan_ip: &str,
    ) -> Result<(String, String), String> {
        let port = self.cfg.llama_port;
        if let Some(n) = serial.strip_prefix("emulator-") {
            let local = 18_000 + n.parse::<u16>().unwrap_or(0) % 1000;
            self.adb(
                &[
                    "-s",
                    serial,
                    "forward",
                    &format!("tcp:{local}"),
                    &format!("tcp:{port}"),
                ],
                SHORT,
            )
            .await?;
            return Ok((
                format!("http://10.0.2.2:{}", self.coordinator_port),
                format!("http://127.0.0.1:{local}"),
            ));
        }
        let ip = if wlan_ip.is_empty() { key } else { wlan_ip };
        if ip.parse::<std::net::IpAddr>().is_err() {
            return Err("no encuentro la IP Wi-Fi del teléfono".into());
        }
        Ok((self.coordinator_url.clone(), format!("http://{ip}:{port}")))
    }

    async fn push(&self, serial: &str, local: &Path, remote: &str) -> Result<(), String> {
        let local = local.to_string_lossy();
        self.adb(&["-s", serial, "push", &local, remote], PUSH)
            .await
            .map(|_| ())
    }

    async fn push_text(&self, serial: &str, text: &str, remote: &str) -> Result<(), String> {
        let tmp = std::env::temp_dir().join(format!("pc-deploy-{}", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        let res = self.push(serial, &tmp, remote).await;
        let _ = std::fs::remove_file(&tmp);
        res
    }

    /// Para el agente anterior (que apaga su llama-server) y arranca el nuevo.
    async fn start(&self, serial: &str, dir: &str) -> Result<(), String> {
        let stop = format!(
            "for p in $(pidof {AGENT}); do kill $p; done; sleep 1; \
             for p in $(pidof {LLAMA}); do kill $p; done; true"
        );
        self.shell(serial, &stop, SHORT).await?;
        self.shell(serial, &format!("sh {dir}/start.sh"), SHORT)
            .await?;
        Ok(())
    }

    async fn finish(&self, serial: &str, key: &str, dir: &str, fp: &str) -> Result<(), String> {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let pid = self
            .shell(serial, &format!("pidof {AGENT}"), SHORT)
            .await
            .unwrap_or_default();
        if pid.trim().is_empty() {
            let log = self
                .shell(serial, &format!("tail -n 5 {dir}/agent.log"), SHORT)
                .await
                .unwrap_or_default();
            return Err(format!("el agente no arrancó: {}", log.trim()));
        }
        self.shell(serial, &format!("echo {fp} > {dir}/.deployed"), SHORT)
            .await?;
        tracing::info!(device = %key, "agente en marcha");
        self.update(key, |v| {
            v.phase = Phase::Running;
            v.detail = Some("Agente en marcha; llama-server cargando el modelo…".into());
        });
        Ok(())
    }
}

fn find_adb(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = configured {
        return p.is_file().then(|| p.to_path_buf());
    }
    let from_path = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("adb"))
            .find(|p| p.is_file())
    });
    if from_path.is_some() {
        return from_path;
    }
    let sdk = std::env::var_os("ANDROID_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Android/sdk"))
        })?;
    Some(sdk.join("platform-tools/adb")).filter(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MDNS: &str = "List of discovered mdns services\n\
        adb-R5CT1234ABC-Xyz123\t_adb-tls-connect._tcp\t192.168.1.21:37159\n\
        adb-R5CT1234ABC-Xyz123\t_adb-tls-pairing._tcp\t192.168.1.21:41011\n\
        adb-9A0B-qq\t_adb-tls-connect._tcp.\t192.168.1.33:40001\n\
        garbage line\n";

    #[test]
    fn parses_mdns_services() {
        let s = parse_mdns(MDNS);
        assert_eq!(s.len(), 3);
        assert!(!s[0].pairing && s[1].pairing);
        assert_eq!(s[1].address, "192.168.1.21:41011");
        assert_eq!(s[2].ip(), "192.168.1.33");
    }

    #[test]
    fn parses_adb_devices_and_keys() {
        let out = "* daemon started successfully\nList of devices attached\n\
            192.168.1.21:37159\tdevice\n\
            adb-9A0B-qq._adb-tls-connect._tcp\tdevice\n\
            emulator-5554\toffline\n\n";
        let d = parse_devices(out);
        assert_eq!(d.len(), 3);
        let services = parse_mdns(MDNS);
        assert_eq!(device_key(&d[0].0, &services), "192.168.1.21");
        assert_eq!(device_key(&d[1].0, &services), "192.168.1.33");
        assert_eq!(device_key(&d[2].0, &services), "emulator-5554");
        assert_eq!(d[2].1, "offline");
    }

    #[test]
    fn node_names() {
        assert_eq!(node_name("Pixel 7", "192.168.1.21"), "pixel-7-21");
        assert_eq!(node_name("SM-A536B", "192.168.1.5"), "sm-a536b-5");
        assert_eq!(node_name("", "emulator-5554"), "telefono-5554");
        assert_eq!(
            node_name("sdk_gphone64_arm64", "emulator-5554"),
            "sdk-gphone64-arm64-5554"
        );
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive() {
        let a = fingerprint(&["x".into(), "y".into()]);
        assert_eq!(a, fingerprint(&["x".into(), "y".into()]));
        assert_ne!(a, fingerprint(&["x".into(), "z".into()]));
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn artifacts_report_what_is_missing() {
        let dir = std::env::temp_dir().join(format!("pc-art-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = Artifacts::find(&dir).unwrap_err();
        assert!(err.contains("pc-agent") && err.contains("gguf"));
        for f in ["pc-agent", "llama-server", "m.gguf"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        let a = Artifacts::find(&dir).unwrap();
        assert_eq!(a.model_file(), "m.gguf");
        assert_eq!(a.stamp().len(), 3);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn agent_config_is_valid_toml_for_the_agent() {
        let file = AgentFile {
            name: "pixel-7-21".into(),
            coordinator_url: "http://192.168.1.50:7800".into(),
            token: "t\"x".into(),
            advertise_url: "http://192.168.1.21:8080".into(),
            llama_url: "http://127.0.0.1:8080".into(),
            heartbeat_secs: 5,
            llama_command: vec![
                "/data/local/tmp/pc/llama-server".into(),
                "-m".into(),
                "m.gguf".into(),
            ],
        };
        let text = toml::to_string(&file).unwrap();
        let back: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(back["token"].as_str(), Some("t\"x"));
        assert_eq!(back["llama_command"].as_array().unwrap().len(), 3);
    }
}
