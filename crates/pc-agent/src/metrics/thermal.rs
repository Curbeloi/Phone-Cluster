//! Temperatura del SoC: máximo de `/sys/class/thermal/thermal_zone*/temp` o,
//! si no se puede leer (usuario `shell` de Android), `dumpsys thermalservice`.

use std::path::Path;

use pc_proto::Thermal;

pub const SYSFS_THERMAL: &str = "/sys/class/thermal";

/// Rango plausible; fuera de él la zona se ignora (sensores rotos o sin datos).
const PLAUSIBLE_C: std::ops::RangeInclusive<f32> = -30.0..=150.0;

/// `/sys` primero; `dumpsys thermalservice` solo si no hay zonas configuradas.
pub async fn read(zones: &[String]) -> Option<Thermal> {
    if let Some(t) = read_max(Path::new(SYSFS_THERMAL), zones) {
        return Some(t);
    }
    if !zones.is_empty() {
        return None;
    }
    let out = super::command_output(
        "dumpsys",
        &["thermalservice"],
        std::time::Duration::from_secs(3),
    )
    .await?;
    parse_dumpsys(&out)
}

/// Máximo de las líneas `Temperature{mValue=…, mType=…}` de la HAL térmica:
/// CPU (0), GPU (1) o SoC (9); si no hay ninguna, cualquiera salvo batería (2).
pub fn parse_dumpsys(text: &str) -> Option<Thermal> {
    let mut soc: Option<f32> = None;
    let mut other: Option<f32> = None;
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("Temperature{") else {
            continue;
        };
        let field = |name: &str| {
            rest.split(", ")
                .find_map(|kv| kv.trim_end_matches('}').strip_prefix(name))
                .map(str::trim)
        };
        let Some(value) = field("mValue=").and_then(|v| v.parse::<f32>().ok()) else {
            continue;
        };
        if !PLAUSIBLE_C.contains(&value) {
            continue;
        }
        let slot = match field("mType=") {
            Some("0" | "1" | "9") => &mut soc,
            Some("2") => continue,
            _ => &mut other,
        };
        *slot = Some(slot.map_or(value, |m| m.max(value)));
    }
    soc.or(other).map(|max_c| Thermal { max_c })
}

/// Máximo de las zonas legibles bajo `base`.
///
/// `zones` vacío = todas. Si no, una zona entra si su directorio
/// (`thermal_zone3`) o su `type` (`cpu-0-0`) aparece en la lista.
/// Las zonas ilegibles o con valores absurdos se ignoran.
pub fn read_max(base: &Path, zones: &[String]) -> Option<Thermal> {
    let entries = std::fs::read_dir(base).ok()?;
    let mut max: Option<f32> = None;
    for entry in entries.flatten() {
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        if !dir_name.starts_with("thermal_zone") {
            continue;
        }
        let path = entry.path();
        if !zones.is_empty() {
            let kind = std::fs::read_to_string(path.join("type")).unwrap_or_default();
            if !zones.iter().any(|z| z == &dir_name || z == kind.trim()) {
                continue;
            }
        }
        let Some(c) = std::fs::read_to_string(path.join("temp"))
            .ok()
            .and_then(|t| parse_temp(&t))
        else {
            continue;
        };
        max = Some(max.map_or(c, |m| m.max(c)));
    }
    max.map(|max_c| Thermal { max_c })
}

/// Interpreta el contenido de un fichero `temp`: miligrados normalmente,
/// grados en algunos kernels.
pub fn parse_temp(text: &str) -> Option<f32> {
    let raw: i64 = text.trim().parse().ok()?;
    #[allow(clippy::cast_precision_loss)]
    let c = if raw.abs() >= 1000 {
        raw as f32 / 1000.0
    } else {
        raw as f32
    };
    PLAUSIBLE_C.contains(&c).then_some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/thermal")
    }

    #[test]
    fn takes_max_of_readable_zones_and_skips_broken_ones() {
        // zone0 = 38.5, zone1 = 41.2, zone2 = basura, zone3 sin temp, zone4 = -40000 (implausible)
        let t = read_max(&base(), &[]).unwrap();
        assert_eq!(t.max_c, 41.2);
    }

    #[test]
    fn filters_by_dir_name_or_type() {
        let t = read_max(&base(), &["thermal_zone0".into()]).unwrap();
        assert_eq!(t.max_c, 38.5);
        let t = read_max(&base(), &["gpu".into()]).unwrap();
        assert_eq!(t.max_c, 41.2);
        assert!(read_max(&base(), &["thermal_zone2".into()]).is_none());
    }

    #[test]
    fn missing_base_is_none() {
        assert!(read_max(Path::new("/nonexistent/thermal"), &[]).is_none());
    }

    #[test]
    fn parses_dumpsys_thermalservice() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dumpsys-thermalservice.txt");
        let t = parse_dumpsys(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(t.max_c, 41.5);
        let only_skin = "Temperature{mValue=33.0, mType=3, mName=skin, mStatus=0}\nTemperature{mValue=50.0, mType=2, mName=battery, mStatus=0}";
        assert_eq!(parse_dumpsys(only_skin).unwrap().max_c, 33.0);
        assert!(parse_dumpsys("Current temperatures from HAL:\n").is_none());
    }

    #[test]
    fn parse_units() {
        assert_eq!(parse_temp("41200\n"), Some(41.2));
        assert_eq!(parse_temp("45"), Some(45.0));
        assert_eq!(parse_temp("-40000"), None);
        assert_eq!(parse_temp("n/a"), None);
    }
}
