//! RAM total y disponible desde `/proc/meminfo`.

use pc_proto::Memory;

pub const PROC_MEMINFO: &str = "/proc/meminfo";

pub fn read() -> Option<Memory> {
    parse_meminfo(&std::fs::read_to_string(PROC_MEMINFO).ok()?)
}

/// Extrae `MemTotal` y `MemAvailable` (en kB) y los pasa a MB.
pub fn parse_meminfo(text: &str) -> Option<Memory> {
    let field = |key: &str| -> Option<u64> {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    Some(Memory {
        total_mb: field("MemTotal")? / 1024,
        available_mb: field("MemAvailable")? / 1024,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixture() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/meminfo");
        let m = parse_meminfo(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(m.total_mb, 7680);
        assert_eq!(m.available_mb, 2100);
    }

    #[test]
    fn missing_available_is_none() {
        assert!(parse_meminfo("MemTotal: 1024 kB\nMemFree: 10 kB\n").is_none());
        assert!(parse_meminfo("").is_none());
    }
}
