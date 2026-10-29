//! Configuración del coordinador (`coordinator.toml`).

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::registry::Thresholds;

/// Token que nunca se imprime.
#[derive(Clone, Default, Deserialize)]
#[serde(transparent)]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// API OpenAI + heartbeats.
    pub listen: SocketAddr,
    /// Dashboard; sin token, así que por defecto solo en loopback.
    pub dashboard_listen: SocketAddr,
    pub token: Secret,
    pub offline_after_secs: u64,
    /// Temperatura de la batería que saca un nodo de rotación (y la de vuelta).
    pub max_temp_c: f32,
    pub resume_temp_c: f32,
    /// Lo mismo para el procesador, que bajo carga se calienta mucho más.
    pub max_soc_temp_c: f32,
    pub resume_soc_temp_c: f32,
    pub min_battery_pct: u8,
    pub max_retries: u32,
    pub require_token_for_inference: bool,
    /// Anunciar el coordinador en la red (Bonjour `_phonecluster._tcp`) para
    /// que la app Android lo encuentre sola.
    pub advertise_mdns: bool,
    /// Descubrimiento e instalación de teléfonos por ADB inalámbrico.
    pub adb: AdbConfig,
    /// Modo dividido: las capas del modelo repartidas entre teléfonos (D-56).
    pub split: SplitConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SplitConfig {
    /// `llama-server` del Mac compilado con RPC; por defecto
    /// `<artifacts_dir>/macos/llama-server`.
    pub llama_bin: Option<PathBuf>,
    /// Puerto local (solo 127.0.0.1) de la cabeza.
    pub head_port: u16,
    pub ctx_size: u32,
}

impl Default for SplitConfig {
    fn default() -> Self {
        Self {
            llama_bin: None,
            head_port: 7810,
            ctx_size: 4096,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdbConfig {
    pub enabled: bool,
    /// Por defecto: `adb` del PATH o del Android SDK.
    pub adb_path: Option<PathBuf>,
    /// Lo que genera `tools/build-android.sh`: pc-agent, llama-server y un `.gguf`.
    pub artifacts_dir: PathBuf,
    /// Carpeta de instalación en el teléfono (escribible por el usuario `shell`).
    pub remote_dir: String,
    pub llama_port: u16,
    /// Argumentos extra para llama-server (contexto, slots, hilos...).
    pub llama_args: Vec<String>,
    /// Nombre del modelo (`--alias`); por defecto, el nombre del fichero sin `.gguf`.
    pub model_alias: Option<String>,
}

impl Default for AdbConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            adb_path: None,
            artifacts_dir: PathBuf::from("android/dist"),
            remote_dir: "/data/local/tmp/phone-cluster".into(),
            llama_port: 8080,
            llama_args: ["-c", "4096", "-np", "2"].map(String::from).to_vec(),
            model_alias: None,
        }
    }
}

impl AdbConfig {
    /// `--alias` del modelo: el configurado o el nombre del fichero sin `.gguf`.
    pub fn alias_for(&self, file_name: &str) -> String {
        self.model_alias
            .clone()
            .unwrap_or_else(|| file_name.trim_end_matches(".gguf").to_string())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 7800)),
            dashboard_listen: SocketAddr::from(([127, 0, 0, 1], 7801)),
            token: Secret::default(),
            offline_after_secs: 15,
            max_temp_c: 45.0,
            resume_temp_c: 40.0,
            max_soc_temp_c: 80.0,
            resume_soc_temp_c: 70.0,
            min_battery_pct: 20,
            max_retries: 2,
            require_token_for_inference: false,
            advertise_mdns: true,
            adb: AdbConfig::default(),
            split: SplitConfig::default(),
        }
    }
}

impl Config {
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        let cfg: Config = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("leyendo {}", path.display()))?;
        Self::from_toml(&text).with_context(|| format!("en {}", path.display()))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.token.expose().trim().is_empty() {
            bail!("token no puede estar vacío");
        }
        if self.resume_soc_temp_c > self.max_soc_temp_c {
            bail!("resume_soc_temp_c debe ser menor o igual que max_soc_temp_c");
        }
        if self.resume_temp_c > self.max_temp_c {
            bail!("resume_temp_c debe ser menor o igual que max_temp_c");
        }
        if self.offline_after_secs == 0 {
            bail!("offline_after_secs debe ser mayor que 0");
        }
        Ok(())
    }

    pub fn thresholds(&self) -> Thresholds {
        Thresholds {
            offline_after: Duration::from_secs(self.offline_after_secs),
            max_temp_c: self.max_temp_c,
            resume_temp_c: self.resume_temp_c,
            max_soc_temp_c: self.max_soc_temp_c,
            resume_soc_temp_c: self.resume_soc_temp_c,
            min_battery_pct: self.min_battery_pct,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_example_parses() {
        let cfg = Config::from_toml(
            r#"
listen = "0.0.0.0:7800"      # API OpenAI + heartbeats
dashboard_listen = "127.0.0.1:7801"
token = "cambia-esto"
offline_after_secs = 15
max_temp_c = 45.0
resume_temp_c = 40.0
min_battery_pct = 20
max_retries = 2
"#,
        )
        .unwrap();
        assert_eq!(cfg.listen.port(), 7800);
        assert_eq!(cfg.token.expose(), "cambia-esto");
        assert!(!cfg.require_token_for_inference);
        assert!(cfg.adb.enabled);
        assert_eq!(cfg.adb.llama_port, 8080);
        let cfg =
            Config::from_toml("token=\"t\"\n[adb]\nenabled=false\nllama_args=[\"-c\",\"2048\"]")
                .unwrap();
        assert!(!cfg.adb.enabled);
        assert_eq!(cfg.adb.llama_args, vec!["-c", "2048"]);
        assert!(!format!("{cfg:?}").contains("cambia-esto"));
    }

    #[test]
    fn example_file_is_valid() {
        let cfg = Config::from_toml(include_str!("../../../coordinator.example.toml")).unwrap();
        assert_eq!(cfg.adb.remote_dir, "/data/local/tmp/phone-cluster");
    }

    #[test]
    fn rejects_empty_token_bad_hysteresis_and_typos() {
        assert!(Config::from_toml("").is_err());
        assert!(Config::from_toml("token=\"t\"\nmax_temp_c=40.0\nresume_temp_c=45.0").is_err());
        assert!(Config::from_toml("token=\"t\"\nmax_temp=40.0").is_err());
    }
}
