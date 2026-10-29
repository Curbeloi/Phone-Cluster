//! Supervisor de los procesos del teléfono: `llama-server` (modo solo) o
//! `rpc-server` (modo dividido), nunca los dos a la vez. Lanza el que toque,
//! lo relanza si muere, lo reinicia con `restart_llama` y cambia de uno a otro
//! cuando el coordinador cambia el modo.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use pc_proto::Mode;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

const MIN_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(10);
/// Si el proceso vivió al menos esto, una caída no cuenta como bucle de fallos.
const STABLE_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub enum Msg {
    Restart,
    SetMode(Mode),
}

/// Un proceso supervisado: comando, log y variables de entorno extra.
#[derive(Debug, Clone)]
pub struct Proc {
    pub command: Vec<String>,
    pub log: PathBuf,
    pub env: Vec<(String, String)>,
}

/// Lo que el supervisor cuenta al heartbeat.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    /// Modo del proceso vivo ahora mismo; `None` si no hay ninguno.
    pub running: Option<Mode>,
    pub last_error: Option<String>,
}

/// Para hablar con el supervisor y saber qué está corriendo.
#[derive(Debug, Clone)]
pub struct Handle {
    pub tx: mpsc::Sender<Msg>,
    pub state: watch::Receiver<State>,
}

/// Lanza el supervisor en `tasks`, empezando en modo solo. Al soltar el
/// `JoinSet` muere también el proceso hijo (`kill_on_drop`).
pub fn spawn(solo: Option<Proc>, worker: Option<Proc>, tasks: &mut JoinSet<()>) -> Handle {
    let (tx, rx) = mpsc::channel(8);
    let (state, watch) = watch::channel(State::default());
    tasks.spawn(run(solo, worker, rx, state));
    Handle { tx, state: watch }
}

/// Cómo terminó la espera sobre un proceso vivo.
enum Exit {
    Died,
    Restart,
    Switch,
    Quit,
}

async fn run(
    solo: Option<Proc>,
    worker: Option<Proc>,
    mut rx: mpsc::Receiver<Msg>,
    state: watch::Sender<State>,
) {
    let set_running = |m: Option<Mode>| state.send_modify(|s| s.running = m);
    let set_error = |e: Option<String>| state.send_modify(|s| s.last_error = e);
    let mut mode = Mode::Solo;
    let mut delay = MIN_DELAY;
    loop {
        set_running(None);
        let proc = match mode {
            Mode::Solo => solo.as_ref(),
            Mode::Worker => worker.as_ref(),
        };
        let Some(proc) = proc else {
            // Nada que ejecutar en este modo: esperar a que cambie.
            if !pause(None, &mut rx, &mut mode).await {
                return;
            }
            delay = MIN_DELAY;
            continue;
        };
        let name = program_name(proc);
        let mut child = match start(proc) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    error = format!("{e:#}"),
                    retry_in_s = delay.as_secs(),
                    "no se pudo lanzar {name}"
                );
                set_error(Some(format!("no se pudo lanzar {name}: {e:#}")));
                if !pause(Some(delay), &mut rx, &mut mode).await {
                    return;
                }
                delay = (delay * 2).min(MAX_DELAY);
                continue;
            }
        };
        let started = Instant::now();
        set_running(Some(mode));
        tracing::info!(pid = ?child.id(), ?mode, log = %proc.log.display(), "{name} lanzado");
        let exit = loop {
            tokio::select! {
                status = child.wait() => {
                    tracing::warn!(status = ?status.as_ref().ok(), retry_in_s = delay.as_secs(), "{name} terminó; se relanza");
                    let how = status.map_or_else(|e| e.to_string(), |s| s.to_string());
                    let last = last_line(&proc.log).map(|l| format!(" · {l}")).unwrap_or_default();
                    set_error(Some(format!("{name} terminó ({how}){last}")));
                    break Exit::Died;
                }
                () = tokio::time::sleep(STABLE_AFTER.saturating_sub(started.elapsed())), if state.borrow().last_error.is_some() => {
                    // Aguantó: el fallo anterior ya no describe el presente.
                    set_error(None);
                }
                msg = rx.recv() => match msg {
                    Some(Msg::SetMode(m)) if m == mode => {}
                    Some(Msg::SetMode(m)) => {
                        tracing::info!(from = ?mode, to = ?m, "cambio de modo");
                        mode = m;
                        break Exit::Switch;
                    }
                    Some(Msg::Restart) => break Exit::Restart,
                    None => break Exit::Quit,
                },
            }
        };
        match exit {
            Exit::Died => {
                set_running(None);
                if started.elapsed() >= STABLE_AFTER {
                    delay = MIN_DELAY;
                }
                let before = mode;
                if !pause(Some(delay), &mut rx, &mut mode).await {
                    return;
                }
                delay = if mode == before {
                    (delay * 2).min(MAX_DELAY)
                } else {
                    MIN_DELAY
                };
            }
            Exit::Restart | Exit::Switch => {
                let _ = child.kill().await;
                if matches!(exit, Exit::Restart) {
                    tracing::info!("{name} reiniciado por comando");
                }
                delay = MIN_DELAY;
            }
            Exit::Quit => {
                let _ = child.kill().await;
                return;
            }
        }
    }
}

/// Espera `delay` (o indefinidamente con `None`) atendiendo mensajes: un
/// cambio de modo corta la espera. Devuelve `false` si el canal se cerró.
async fn pause(delay: Option<Duration>, rx: &mut mpsc::Receiver<Msg>, mode: &mut Mode) -> bool {
    let sleep = async {
        match delay {
            Some(d) => tokio::time::sleep(d).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            () = &mut sleep => return true,
            msg = rx.recv() => match msg {
                Some(Msg::SetMode(m)) if m != *mode => {
                    tracing::info!(from = ?*mode, to = ?m, "cambio de modo");
                    *mode = m;
                    return true;
                }
                Some(_) => {}
                None => return false,
            },
        }
    }
}

/// Última línea no vacía del log (lee solo el final), recortada.
fn last_line(log: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(log).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(4096))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let line = text.lines().rev().map(str::trim).find(|l| !l.is_empty())?;
    Some(line.chars().take(200).collect())
}

fn program_name(proc: &Proc) -> String {
    proc.command
        .first()
        .and_then(|p| Path::new(p).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "proceso".into())
}

fn start(proc: &Proc) -> anyhow::Result<Child> {
    let Some((program, args)) = proc.command.split_first() else {
        bail!("el comando está vacío");
    };
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&proc.log)
        .with_context(|| format!("abriendo {}", proc.log.display()))?;
    let err = out.try_clone()?;
    Command::new(program)
        .args(args)
        .envs(proc.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("ejecutando {program}"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn log() -> PathBuf {
        std::env::temp_dir().join(format!("pc-agent-sup-{}.log", uuid::Uuid::new_v4()))
    }

    /// Cuenta lanzamientos haciendo que cada hijo añada una línea al log.
    fn launches(log: &Path) -> usize {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .count()
    }

    fn sh(script: &str, log: &Path) -> Proc {
        Proc {
            command: vec!["sh".into(), "-c".into(), script.into()],
            log: log.to_path_buf(),
            env: vec![("PC_TEST".into(), "x".into())],
        }
    }

    #[tokio::test]
    async fn relaunches_a_process_that_dies() {
        let log = log();
        let mut tasks = JoinSet::new();
        let _h = spawn(Some(sh("echo up; exit 1", &log)), None, &mut tasks);
        tokio::time::sleep(Duration::from_millis(1600)).await;
        assert!(launches(&log) >= 2, "lanzado {} veces", launches(&log));
        drop(tasks);
        let _ = std::fs::remove_file(log);
    }

    #[tokio::test]
    async fn reports_why_the_process_died() {
        let log = log();
        let mut tasks = JoinSet::new();
        let h = spawn(
            Some(sh("echo cargando; echo 'error: sin memoria'; exit 3", &log)),
            None,
            &mut tasks,
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let err = h.state.borrow().last_error.clone().unwrap();
        assert!(err.contains("exit status: 3"), "{err}");
        assert!(err.ends_with("error: sin memoria"), "{err}");
        drop(tasks);
        let _ = std::fs::remove_file(log);
    }

    #[tokio::test]
    async fn restart_command_kills_and_relaunches() {
        let log = log();
        let mut tasks = JoinSet::new();
        let h = spawn(Some(sh("echo up; exec sleep 30", &log)), None, &mut tasks);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(launches(&log), 1);
        h.tx.send(Msg::Restart).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(launches(&log), 2, "se relanza sin esperar");
        drop(tasks);
        let _ = std::fs::remove_file(log);
    }

    #[tokio::test]
    async fn switches_between_solo_and_worker() {
        let (solo_log, worker_log) = (log(), log());
        let mut tasks = JoinSet::new();
        let h = spawn(
            Some(sh("echo solo; exec sleep 30", &solo_log)),
            Some(sh("echo \"$PC_TEST\"; exec sleep 30", &worker_log)),
            &mut tasks,
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(h.state.borrow().running, Some(Mode::Solo));
        // Repetir el modo actual no reinicia nada.
        h.tx.send(Msg::SetMode(Mode::Solo)).await.unwrap();
        h.tx.send(Msg::SetMode(Mode::Worker)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(h.state.borrow().running, Some(Mode::Worker));
        assert_eq!(launches(&solo_log), 1);
        assert_eq!(
            std::fs::read_to_string(&worker_log).unwrap(),
            "x\n",
            "con su entorno"
        );
        h.tx.send(Msg::SetMode(Mode::Solo)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(h.state.borrow().running, Some(Mode::Solo));
        assert_eq!(launches(&solo_log), 2);
        drop(tasks);
        let _ = std::fs::remove_file(solo_log);
        let _ = std::fs::remove_file(worker_log);
    }

    #[tokio::test]
    async fn worker_without_rpc_command_runs_nothing() {
        let log = log();
        let mut tasks = JoinSet::new();
        let h = spawn(Some(sh("echo up; exec sleep 30", &log)), None, &mut tasks);
        tokio::time::sleep(Duration::from_millis(300)).await;
        h.tx.send(Msg::SetMode(Mode::Worker)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(h.state.borrow().running, None);
        drop(tasks);
        let _ = std::fs::remove_file(log);
    }

    #[tokio::test]
    async fn missing_binary_does_not_panic() {
        let log = log();
        let mut tasks = JoinSet::new();
        let proc = Proc {
            command: vec!["/nonexistent/llama-server".into()],
            log: log.clone(),
            env: Vec::new(),
        };
        let _h = spawn(Some(proc), None, &mut tasks);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!tasks.is_empty());
        let _ = std::fs::remove_file(log);
    }
}
