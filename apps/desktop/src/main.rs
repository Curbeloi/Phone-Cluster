//! App de Mac de Phone Cluster. Arranca el coordinador dentro de la propia app
//! y muestra el dashboard en su ventana: no hacen falta terminal ni
//! `coordinator.toml` a mano. La primera vez crea la configuración, copia lo
//! que se instala en los teléfonos y descarga el modelo.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use pc_coordinator::config::Config;
use serde::Serialize;
use tauri::{AppHandle, Manager};

const MODEL_URL: &str = "https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/main/qwen2.5-1.5b-instruct-q4_k_m.gguf";
const EXAMPLE_CONFIG: &str = include_str!("../../../coordinator.example.toml");
const EXAMPLE_ARTIFACTS: &str =
    "artifacts_dir = \"android/dist\"          # lo genera tools/build-android.sh";
/// Lo que la app lleva dentro y copia a la carpeta de artefactos: la app
/// Android (que el QR descarga), los binarios para la instalación por ADB y
/// la cabeza del modo dividido, que corre en este Mac.
const BUNDLED: [&str; 4] = [
    "phone-cluster.apk",
    "pc-agent",
    "llama-server",
    "macos/llama-server",
];

/// Lo que muestra la pantalla de arranque mientras tanto.
#[derive(Debug, Clone, Default, Serialize)]
struct Boot {
    stage: String,
    /// 0..1 durante la descarga del modelo.
    progress: Option<f64>,
    error: Option<String>,
    log: Option<String>,
}

#[derive(Default)]
struct BootState(Mutex<Boot>);

impl BootState {
    fn update(&self, f: impl FnOnce(&mut Boot)) {
        f(&mut self.0.lock().unwrap_or_else(|e| e.into_inner()));
    }
    fn stage(&self, text: &str) {
        self.update(|b| {
            b.stage = text.into();
            b.progress = None;
        });
    }
}

#[tauri::command]
fn status(state: tauri::State<'_, Arc<BootState>>) -> Boot {
    state.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn main() {
    let boot = Arc::new(BootState::default());
    let result = tauri::Builder::default()
        .manage(boot.clone())
        .invoke_handler(tauri::generate_handler![status])
        .setup(move |app| {
            let handle = app.handle().clone();
            let boot = boot.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = start(&handle, &boot).await {
                    tracing::error!(error = format!("{e:#}"), "no se pudo arrancar");
                    boot.update(|b| b.error = Some(format!("{e:#}")));
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("Phone Cluster: {e}");
        std::process::exit(1);
    }
}

async fn start(app: &AppHandle, boot: &BootState) -> anyhow::Result<()> {
    let data = app
        .path()
        .app_data_dir()
        .context("no encuentro la carpeta de datos de la app")?;
    std::fs::create_dir_all(&data).with_context(|| format!("creando {}", data.display()))?;
    let log = data.join("coordinator.log");
    init_logging(&log);
    boot.update(|b| b.log = Some(log.display().to_string()));

    boot.stage("Preparando la configuración…");
    let cfg = load_or_create_config(&data)?;

    boot.stage("Preparando lo que se instala en los teléfonos…");
    let artifacts = cfg.adb.artifacts_dir.clone();
    std::fs::create_dir_all(&artifacts)
        .with_context(|| format!("creando {}", artifacts.display()))?;
    copy_bundled_binaries(app, &artifacts)?;

    boot.stage("Arrancando el coordinador…");
    let api = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| busy(cfg.listen.port()))?;
    let dashboard = tokio::net::TcpListener::bind(cfg.dashboard_listen)
        .await
        .with_context(|| busy(cfg.dashboard_listen.port()))?;
    let dashboard_url = format!("http://{}/", dashboard.local_addr()?);
    tauri::async_runtime::spawn(async move {
        if let Err(e) = pc_coordinator::serve(cfg, api, dashboard).await {
            tracing::error!(error = format!("{e:#}"), "el coordinador se detuvo");
        }
    });

    ensure_model(&artifacts, boot).await?;

    boot.stage("Abriendo el panel…");
    let url = tauri::Url::parse(&dashboard_url)?;
    app.get_webview_window("main")
        .context("no encuentro la ventana principal")?
        .navigate(url)?;
    Ok(())
}

fn busy(port: u16) -> String {
    format!(
        "El puerto {port} está ocupado. ¿Hay otro coordinador abierto, por ejemplo en una terminal? Ciérralo y vuelve a abrir la app."
    )
}

/// Logs a un fichero en la carpeta de datos (la app no tiene terminal).
fn init_logging(path: &Path) {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    if let Ok(file) = file {
        let _ = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter(filter)
            .with_writer(Mutex::new(file))
            .try_init();
    }
}

/// La primera vez escribe `coordinator.toml` a partir del ejemplo, con un
/// token aleatorio y la carpeta de artefactos dentro de la carpeta de datos.
fn load_or_create_config(data: &Path) -> anyhow::Result<Config> {
    let path = data.join("coordinator.toml");
    if !path.exists() {
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let artifacts = serde_json::to_string(&data.join("android").display().to_string())?;
        if !EXAMPLE_CONFIG.contains("token = \"cambia-esto\"")
            || !EXAMPLE_CONFIG.contains(EXAMPLE_ARTIFACTS)
        {
            bail!("coordinator.example.toml cambió y la app no sabe rellenarlo");
        }
        let text = EXAMPLE_CONFIG
            .replace("token = \"cambia-esto\"", &format!("token = \"{token}\""))
            .replace(
                EXAMPLE_ARTIFACTS,
                &format!("artifacts_dir = {artifacts}   # la app copia aquí pc-agent y llama-server y descarga el modelo"),
            );
        std::fs::write(&path, text).with_context(|| format!("escribiendo {}", path.display()))?;
        tracing::info!(path = %path.display(), "configuración creada");
    }
    let mut cfg = Config::load(&path)?;
    if cfg.adb.artifacts_dir.is_relative() {
        cfg.adb.artifacts_dir = data.join(&cfg.adb.artifacts_dir);
    }
    Ok(cfg)
}

/// Carpetas donde buscar `pc-agent` y `llama-server`: dentro del `.app` y,
/// al ejecutar con `cargo run`, `android/dist` del proyecto.
fn binary_sources(app: &AppHandle) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(res) = app.path().resource_dir() {
        dirs.push(res.join("android"));
    }
    dirs.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../android/dist"));
    dirs
}

/// Copia los binarios solo si cambiaron: la fecha del fichero forma parte de
/// la huella de instalación, así que copiarlos siempre reinstalaría los
/// teléfonos en cada arranque.
fn copy_bundled_binaries(app: &AppHandle, dest: &Path) -> anyhow::Result<()> {
    let sources = binary_sources(app);
    for name in BUNDLED {
        let src = sources
            .iter()
            .map(|d| d.join(name))
            .find(|p| p.is_file())
            .with_context(|| {
                format!("la app no incluye {name}: compílala tras ejecutar tools/build-android.sh")
            })?;
        let dst = dest.join(name);
        if same_content(&src, &dst) {
            continue;
        }
        if let Some(dir) = dst.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creando {}", dir.display()))?;
        }
        std::fs::copy(&src, &dst).with_context(|| format!("copiando {name}"))?;
        #[cfg(unix)]
        if !name.ends_with(".apk") {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))?;
        }
        tracing::info!(name, "binario para Android actualizado");
    }
    Ok(())
}

fn same_content(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    ma.len() == mb.len() && std::fs::read(a).ok() == std::fs::read(b).ok()
}

fn find_gguf(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "gguf"))
}

/// Deja un `.gguf` en `dir`: reutiliza el del proyecto si existe (desarrollo)
/// o lo descarga mostrando el progreso.
async fn ensure_model(dir: &Path, boot: &BootState) -> anyhow::Result<()> {
    if find_gguf(dir).is_some() {
        return Ok(());
    }
    let dev = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../android/dist");
    if let Some(src) = find_gguf(&dev) {
        let dst = dir.join(src.file_name().context("nombre de modelo inválido")?);
        boot.stage("Copiando el modelo del proyecto…");
        if std::fs::hard_link(&src, &dst).is_err() {
            std::fs::copy(&src, &dst).context("copiando el modelo")?;
        }
        return Ok(());
    }

    let name = MODEL_URL.rsplit('/').next().unwrap_or("model.gguf");
    let part = dir.join(format!("{name}.part"));
    let total = content_length(MODEL_URL).await;
    let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if total.is_none_or(|t| have < t) {
        // curl va envuelto en un sh que lo mata si la app se cierra: cerrarla no
        // ejecuta destructores y kill_on_drop no llegaría a actuar.
        const SCRIPT: &str = r#"curl "$@" & c=$!
trap 'kill $c 2>/dev/null' EXIT INT TERM
while kill -0 "$c" 2>/dev/null; do
  kill -0 "$PPID" 2>/dev/null || exit 1
  sleep 1
done
trap - EXIT
wait "$c""#;
        let mut child = tokio::process::Command::new("/bin/sh")
            .args(["-c", SCRIPT, "sh"])
            .args([
                "-fsSL",
                "--retry",
                "10",
                "--retry-all-errors",
                "--retry-delay",
                "2",
                "--connect-timeout",
                "20",
                "--speed-limit",
                "1024",
                "--speed-time",
                "60",
                "-C",
                "-",
                "-o",
            ])
            .arg(&part)
            .arg(MODEL_URL)
            .kill_on_drop(true)
            .spawn()
            .context("no se pudo ejecutar curl")?;
        let mb = total.map(|t| t / (1024 * 1024));
        let status = loop {
            tokio::select! {
                status = child.wait() => break status?,
                () = tokio::time::sleep(Duration::from_millis(500)) => {
                    let got = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                    boot.update(|b| {
                        b.stage = match mb {
                            Some(mb) => format!("Descargando el modelo ({mb} MB). Solo la primera vez…"),
                            None => "Descargando el modelo. Solo la primera vez…".into(),
                        };
                        #[allow(clippy::cast_precision_loss)]
                        let p = total.map(|t| got as f64 / t.max(1) as f64);
                        b.progress = p;
                    });
                }
            }
        };
        if !status.success() {
            bail!(
                "No se pudo descargar el modelo. ¿Hay conexión a internet? Vuelve a abrir la app para reintentar (continúa donde se quedó)."
            );
        }
    }
    let got = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if let Some(t) = total
        && got != t
    {
        // Un .part más grande de lo esperado no se puede reanudar: empezar de cero.
        if got > t {
            let _ = std::fs::remove_file(&part);
        }
        bail!(
            "La descarga del modelo quedó incompleta ({got} de {t} bytes). Vuelve a abrir la app para continuar."
        );
    }
    std::fs::rename(&part, dir.join(name)).context("guardando el modelo")?;
    tracing::info!(name, "modelo descargado");
    Ok(())
}

/// Tamaño del fichero remoto, siguiendo redirecciones (`curl -sIL`).
async fn content_length(url: &str) -> Option<u64> {
    let out = tokio::process::Command::new("curl")
        .args(["-sIL", "--max-time", "20", url])
        .output()
        .await
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        // Tras las redirecciones, la última cabecera es la del fichero final.
        .next_back()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_has_the_lines_the_app_rewrites() {
        assert!(EXAMPLE_CONFIG.contains("token = \"cambia-esto\""));
        assert!(EXAMPLE_CONFIG.contains(EXAMPLE_ARTIFACTS));
    }

    #[test]
    fn creates_config_with_random_token_and_absolute_artifacts() {
        let dir = std::env::temp_dir().join(format!("pc-desktop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = load_or_create_config(&dir).unwrap();
        assert_ne!(cfg.token.expose(), "cambia-esto");
        assert_eq!(cfg.token.expose().len(), 64);
        assert_eq!(cfg.adb.artifacts_dir, dir.join("android"));
        // La segunda vez se respeta el fichero existente.
        let again = load_or_create_config(&dir).unwrap();
        assert_eq!(again.token.expose(), cfg.token.expose());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn same_content_compares_bytes() {
        let dir = std::env::temp_dir().join(format!("pc-desktop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::write(&a, "xy").unwrap();
        assert!(!same_content(&a, &b));
        std::fs::write(&b, "xz").unwrap();
        assert!(!same_content(&a, &b));
        std::fs::write(&b, "xy").unwrap();
        assert!(same_content(&a, &b));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
