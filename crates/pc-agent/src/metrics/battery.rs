//! Batería: `battery_file` (la app Android), `termux-battery-status`, luego `/sys/class/power_supply/battery` y,
//! sin Termux (agente instalado por ADB), `dumpsys battery`.

use std::path::Path;
use std::time::Duration;

use pc_proto::Battery;
use serde::Deserialize;

pub const SYSFS_BATTERY: &str = "/sys/class/power_supply/battery";
const TERMUX_TIMEOUT: Duration = Duration::from_secs(3);

/// Lee la batería. Nunca tarda más de ~3 s; si no hay ninguna fuente, `None`.
pub async fn read(file: Option<&Path>) -> Option<Battery> {
    if let Some(path) = file {
        match std::fs::read_to_string(path)
            .ok()
            .and_then(|t| parse_termux(&t))
        {
            Some(b) => return Some(b),
            None => tracing::debug!(path = %path.display(), "battery_file ilegible"),
        }
    }
    if let Some(b) = read_termux().await {
        return Some(b);
    }
    if let Some(b) = read_sysfs(Path::new(SYSFS_BATTERY)) {
        return Some(b);
    }
    let out = super::command_output("dumpsys", &["battery"], TERMUX_TIMEOUT).await?;
    parse_dumpsys(&out)
}

/// Interpreta `dumpsys battery`: `level`, `* powered` y `temperature` (décimas de °C).
pub fn parse_dumpsys(text: &str) -> Option<Battery> {
    let mut level = None;
    let mut scale: i64 = 100;
    let mut powered = false;
    let mut temp_c = None;
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key {
            "level" => level = value.parse::<i64>().ok(),
            "scale" => scale = value.parse().unwrap_or(100).max(1),
            "temperature" => temp_c = value.parse::<f32>().ok().map(|t| t / 10.0),
            k if k.ends_with(" powered") => powered |= value == "true",
            _ => {}
        }
    }
    let pct = u8::try_from((level? * 100 / scale).clamp(0, 100)).ok()?;
    Some(Battery {
        pct,
        charging: powered,
        temp_c: temp_c.filter(|c| (-30.0..=100.0).contains(c)),
    })
}

async fn read_termux() -> Option<Battery> {
    let out = super::command_output("termux-battery-status", &[], TERMUX_TIMEOUT).await?;
    let parsed = parse_termux(&out);
    if parsed.is_none() {
        tracing::debug!("termux-battery-status devolvió JSON inesperado");
    }
    parsed
}

#[derive(Deserialize)]
struct TermuxBattery {
    percentage: Option<i64>,
    plugged: Option<String>,
    status: Option<String>,
    temperature: Option<f32>,
}

/// Interpreta la salida JSON de `termux-battery-status`.
///
/// "Cargando" significa enchufado (`plugged` distinto de `UNPLUGGED`), porque
/// con límite de carga al 80 % Android informa `NOT_CHARGING` aunque esté
/// conectado. Sin `plugged`, se usa `status` (ver DECISIONS.md, D-08).
pub fn parse_termux(json: &str) -> Option<Battery> {
    let t: TermuxBattery = serde_json::from_str(json).ok()?;
    let pct = u8::try_from(t.percentage?.clamp(0, 100)).ok()?;
    let charging = match t.plugged.as_deref() {
        Some(p) => !p.eq_ignore_ascii_case("UNPLUGGED"),
        None => matches!(t.status.as_deref(), Some("CHARGING" | "FULL")),
    };
    Some(Battery {
        pct,
        charging,
        temp_c: t.temperature.filter(|c| (-30.0..=100.0).contains(c)),
    })
}

/// Lee `capacity`, `status` y `temp` (décimas de °C) bajo `dir`.
pub fn read_sysfs(dir: &Path) -> Option<Battery> {
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
    let pct: i64 = read("capacity")?.trim().parse().ok()?;
    let pct = u8::try_from(pct.clamp(0, 100)).ok()?;
    let charging = read("status")
        .map(|s| matches!(s.trim(), "Charging" | "Full"))
        .unwrap_or(false);
    let temp_c = read("temp")
        .and_then(|s| s.trim().parse::<f32>().ok())
        .map(|t| t / 10.0)
        .filter(|c| (-30.0..=100.0).contains(c));
    Some(Battery {
        pct,
        charging,
        temp_c,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn parses_termux_output() {
        let text = std::fs::read_to_string(fixture("termux-battery-status.json")).unwrap();
        let b = parse_termux(&text).unwrap();
        assert_eq!(b.pct, 82);
        assert!(b.charging);
        assert_eq!(b.temp_c, Some(34.5));
    }

    #[test]
    fn termux_unplugged_and_not_charging_plugged() {
        let b = parse_termux(
            r#"{"percentage":15,"plugged":"UNPLUGGED","status":"DISCHARGING","temperature":30.1}"#,
        )
        .unwrap();
        assert!(!b.charging);
        let b = parse_termux(r#"{"percentage":80,"plugged":"PLUGGED_AC","status":"NOT_CHARGING"}"#)
            .unwrap();
        assert!(b.charging);
        assert_eq!(b.temp_c, None);
    }

    #[test]
    fn termux_garbage_is_none() {
        assert!(parse_termux("not json").is_none());
        assert!(parse_termux(r#"{"status":"FULL"}"#).is_none());
    }

    #[test]
    fn parses_dumpsys_fixture() {
        let text = std::fs::read_to_string(fixture("dumpsys-battery.txt")).unwrap();
        let b = parse_dumpsys(&text).unwrap();
        assert_eq!(b.pct, 85);
        assert!(b.charging);
        assert_eq!(b.temp_c, Some(28.7));
        assert!(parse_dumpsys("Current Battery Service state:\n").is_none());
    }

    #[test]
    fn reads_sysfs_fixture() {
        let b = read_sysfs(&fixture("power_supply/battery")).unwrap();
        assert_eq!(b.pct, 64);
        assert!(!b.charging);
        assert_eq!(b.temp_c, Some(31.2));
    }

    #[test]
    fn missing_sysfs_is_none() {
        assert!(read_sysfs(Path::new("/nonexistent/battery")).is_none());
    }
}
