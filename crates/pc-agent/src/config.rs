//! Configuración del agente: fichero TOML, sobreescrito por variables
//! `PC_AGENT_*` y por flags de línea de comandos (en ese orden de prioridad).

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::Parser;
use serde::Deserialize;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Agente de Phone Cluster: reporta el teléfono y su llama-server"
)]
pub struct Cli {
    /// Ruta del fichero de configuración (por defecto ~/.config/pc-agent/config.toml).
    #[arg(long, env = "PC_AGENT_CONFIG")]
    pub config: Option<PathBuf>,
    #[arg(long, env = "PC_AGENT_NAME")]
    pub name: Option<String>,
    #[arg(long, env = "PC_AGENT_COORDINATOR_URL")]
    pub coordinator_url: Option<String>,
    #[arg(long, env = "PC_AGENT_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
    #[arg(long, env = "PC_AGENT_ADVERTISE_URL")]
    pub advertise_url: Option<String>,
    #[arg(long, env = "PC_AGENT_LLAMA_URL")]
    pub llama_url: Option<String>,
    #[arg(long, env = "PC_AGENT_HEARTBEAT_SECS")]
    pub heartbeat_secs: Option<u64>,
    /// Zonas térmicas a leer, separadas por comas (vacío = todas las legibles).
    #[arg(long, env = "PC_AGENT_THERMAL_ZONES", value_delimiter = ',')]
    pub thermal_zones: Option<Vec<String>>,
    /// Recolecta una vez, imprime el heartbeat por stdout y termina.
    #[arg(long)]
    pub once: bool,
    /// Métricas sintéticas del teléfono, para correr varios agentes en el Mac.
    #[arg(long)]
    pub mock: bool,
    /// Con --mock: temperatura de la batería simulada (°C); el SoC va 10 °C por encima.
    #[arg(long, default_value_t = 36.0)]
    pub mock_temp: f32,
    /// Con --mock: porcentaje de batería simulado.
    #[arg(long, default_value_t = 80)]
    pub mock_battery: u8,
    /// Con --mock: el teléfono simulado está cargando.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub mock_charging: bool,
    /// Ejecutable de `rpc-server` para el modo dividido (sustituye a `rpc_command`).
    #[arg(long, env = "PC_AGENT_RPC_SERVER")]
    pub rpc_server: Option<String>,
    /// Puerto de `rpc-server` (por defecto 50052).
    #[arg(long, env = "PC_AGENT_RPC_PORT")]
    pub rpc_port: Option<u16>,
    /// Comando para lanzar y supervisar llama-server, tras `--`
    /// (p. ej. `-- llama-server -m modelo.gguf --host 0.0.0.0`).
    #[arg(last = true)]
    pub llama_command: Vec<String>,
}

/// Contenido de `config.toml`; todo es opcional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    name: Option<String>,
    coordinator_url: Option<String>,
    token: Option<String>,
    advertise_url: Option<String>,
    llama_url: Option<String>,
    heartbeat_secs: Option<u64>,
    thermal_zones: Option<Vec<String>>,
    llama_command: Option<Vec<String>>,
    rpc_command: Option<Vec<String>>,
    rpc_port: Option<u16>,
    battery_file: Option<PathBuf>,
}

/// Token que nunca se imprime en logs ni en `Debug`.
#[derive(Clone, Default)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"***\"")
    }
}

#[derive(Debug, Clone)]
pub struct MockMetrics {
    pub temp_c: f32,
    pub battery_pct: u8,
    pub charging: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub coordinator_url: String,
    pub token: Secret,
    /// `None` = detectar la IP de la interfaz que llega al coordinador.
    pub advertise_url: Option<String>,
    pub llama_url: String,
    pub heartbeat_secs: u64,
    pub thermal_zones: Vec<String>,
    pub once: bool,
    /// `Some` en modo `--mock`.
    pub mock: Option<MockMetrics>,
    /// Directorio donde se guardan `node_id` y `llama-server.log`.
    pub state_dir: PathBuf,
    /// Si no está vacío, el agente lanza y supervisa llama-server con él.
    pub llama_command: Vec<String>,
    /// `rpc-server` y sus opciones, sin `-H` ni `-p`: el agente los añade.
    /// Vacío = este teléfono no puede entrar en el modo dividido.
    pub rpc_command: Vec<String>,
    pub rpc_port: u16,
    /// JSON con el formato de `termux-battery-status` que escribe otro
    /// proceso (la app Android). Tiene prioridad sobre las demás fuentes.
    pub battery_file: Option<PathBuf>,
}

impl Config {
    /// Valores por defecto, útiles en tests y como base del fichero.
    pub fn new(name: impl Into<String>, coordinator_url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            coordinator_url: coordinator_url.into(),
            token: Secret::default(),
            advertise_url: None,
            llama_url: "http://127.0.0.1:8080".into(),
            heartbeat_secs: 5,
            thermal_zones: Vec::new(),
            once: false,
            mock: None,
            state_dir: default_dir(),
            llama_command: Vec::new(),
            rpc_command: Vec::new(),
            rpc_port: 50052,
            battery_file: None,
        }
    }

    pub fn load(cli: Cli) -> anyhow::Result<Self> {
        let path = cli
            .config
            .clone()
            .unwrap_or_else(|| default_dir().join("config.toml"));
        let file = read_file(&path, cli.config.is_some())?;
        let state_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(default_dir);
        let mut cfg = Config::new(
            pick(cli.name, file.name).unwrap_or_else(|| "phone".into()),
            pick(cli.coordinator_url, file.coordinator_url)
                .unwrap_or_else(|| "http://127.0.0.1:7800".into()),
        );
        cfg.token = Secret::new(pick(cli.token, file.token).unwrap_or_default());
        cfg.advertise_url = pick(cli.advertise_url, file.advertise_url);
        if let Some(url) = pick(cli.llama_url, file.llama_url) {
            cfg.llama_url = url;
        }
        if let Some(secs) = pick(cli.heartbeat_secs, file.heartbeat_secs) {
            cfg.heartbeat_secs = secs.max(1);
        }
        cfg.thermal_zones = pick(cli.thermal_zones, file.thermal_zones).unwrap_or_default();
        cfg.thermal_zones.retain(|z| !z.trim().is_empty());
        cfg.llama_command = if cli.llama_command.is_empty() {
            file.llama_command.unwrap_or_default()
        } else {
            cli.llama_command
        };
        cfg.rpc_command = match cli.rpc_server {
            Some(bin) => vec![bin],
            None => file.rpc_command.unwrap_or_default(),
        };
        if let Some(port) = pick(cli.rpc_port, file.rpc_port) {
            cfg.rpc_port = port;
        }
        cfg.battery_file = file.battery_file;
        cfg.once = cli.once;
        cfg.state_dir = state_dir;
        if cli.mock {
            cfg.mock = Some(MockMetrics {
                temp_c: cli.mock_temp,
                battery_pct: cli.mock_battery.min(100),
                charging: cli.mock_charging,
            });
        }
        if cfg.name.trim().is_empty() {
            bail!("name no puede estar vacío");
        }
        Ok(cfg)
    }
}

fn pick<T>(flag_or_env: Option<T>, file: Option<T>) -> Option<T> {
    flag_or_env.or(file)
}

fn default_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| ".".into());
    home.join(".config").join("pc-agent")
}

fn read_file(path: &Path, explicit: bool) -> anyhow::Result<FileConfig> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("leyendo {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => {
            tracing::debug!(path = %path.display(), "sin fichero de configuración, usando valores por defecto");
            Ok(FileConfig::default())
        }
        Err(e) => Err(e).with_context(|| format!("leyendo {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        let mut all = vec!["pc-agent"];
        all.extend_from_slice(args);
        Cli::parse_from(all)
    }

    #[test]
    fn flags_override_file() {
        let dir = std::env::temp_dir().join(format!("pc-agent-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
name = "pixel-7"
coordinator_url = "http://192.168.1.50:7800"
token = "cambia-esto"
llama_url = "http://127.0.0.1:8080"
heartbeat_secs = 5
thermal_zones = ["thermal_zone3"]
llama_command = ["llama-server", "-m", "m.gguf"]
rpc_command = ["rpc-server", "-c"]
"#,
        )
        .unwrap();
        let cfg =
            Config::load(cli(&["--config", path.to_str().unwrap(), "--name", "otro"])).unwrap();
        assert_eq!(cfg.name, "otro");
        assert_eq!(cfg.coordinator_url, "http://192.168.1.50:7800");
        assert_eq!(cfg.token.expose(), "cambia-esto");
        assert_eq!(cfg.thermal_zones, vec!["thermal_zone3"]);
        assert_eq!(cfg.llama_command, vec!["llama-server", "-m", "m.gguf"]);
        assert_eq!(cfg.rpc_command, vec!["rpc-server", "-c"]);
        assert_eq!(cfg.rpc_port, 50052);
        assert_eq!(cfg.state_dir, dir);
        assert!(cfg.mock.is_none());
        assert!(!format!("{cfg:?}").contains("cambia-esto"));
    }

    #[test]
    fn explicit_missing_file_is_an_error_but_default_is_not() {
        assert!(Config::load(cli(&["--config", "/nonexistent/pc-agent.toml"])).is_err());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = toml::from_str::<FileConfig>("nmae = \"typo\"").unwrap_err();
        assert!(err.to_string().contains("nmae"));
    }

    #[test]
    fn mock_flags() {
        let dir = std::env::temp_dir().join(format!("pc-agent-mock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "").unwrap();
        let cfg = Config::load(cli(&[
            "--config",
            path.to_str().unwrap(),
            "--mock",
            "--mock-temp",
            "46",
            "--mock-charging",
            "false",
            "--",
            "mock-llama",
            "--port",
            "9201",
        ]))
        .unwrap();
        assert_eq!(cfg.llama_command, vec!["mock-llama", "--port", "9201"]);
        let m = cfg.mock.unwrap();
        assert_eq!(m.temp_c, 46.0);
        assert!(!m.charging);
    }
}
