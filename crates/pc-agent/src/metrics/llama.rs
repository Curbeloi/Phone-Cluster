//! Consultas al `llama-server` local: `/health`, `/props` y `/slots`.
//!
//! Campos verificados contra llama.cpp b11963 (ver DECISIONS.md, D-05).

use std::time::Duration;

use pc_proto::LlamaStatus;
use reqwest::StatusCode;
use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(3);

/// Lo que se sabe de `/props`; se conserva el último valor si falla.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Props {
    pub model: Option<String>,
    pub ctx_size: Option<u32>,
    pub slots_total: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Slots {
    pub total: u32,
    pub busy: u32,
}

pub async fn health(client: &reqwest::Client, base: &str) -> LlamaStatus {
    let res = client
        .get(url(base, "/health"))
        .timeout(TIMEOUT)
        .send()
        .await;
    match res {
        Err(_) => LlamaStatus::Down,
        Ok(r) if r.status().is_success() => LlamaStatus::Ok,
        Ok(r) if r.status() == StatusCode::SERVICE_UNAVAILABLE => LlamaStatus::Loading,
        Ok(_) => LlamaStatus::Error,
    }
}

pub async fn props(client: &reqwest::Client, base: &str) -> Option<Props> {
    let v = get_json(client, base, "/props").await?;
    Some(parse_props(&v))
}

pub async fn slots(client: &reqwest::Client, base: &str) -> Option<Slots> {
    let v = get_json(client, base, "/slots").await?;
    parse_slots(&v)
}

async fn get_json(client: &reqwest::Client, base: &str, path: &str) -> Option<Value> {
    let res = client
        .get(url(base, path))
        .timeout(TIMEOUT)
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        tracing::debug!(path, status = %res.status(), "llama-server respondió con error");
        return None;
    }
    res.json().await.ok()
}

fn url(base: &str, path: &str) -> String {
    format!("{}{path}", base.trim_end_matches('/'))
}

/// `model_alias` es el nombre que llama-server anuncia en `/v1/models`
/// (primer `--alias`, nombre del modelo o nombre del fichero).
pub fn parse_props(v: &Value) -> Props {
    let model = v
        .get("model_alias")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            let path = v.get("model_path")?.as_str()?;
            let file = path.rsplit(['/', '\\']).next()?;
            Some(file.trim_end_matches(".gguf").to_owned()).filter(|s| !s.is_empty())
        });
    let ctx_size = v
        .pointer("/default_generation_settings/n_ctx")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    let slots_total = v
        .get("total_slots")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    Props {
        model,
        ctx_size,
        slots_total,
    }
}

/// `/slots` devuelve un array con un objeto por slot y `is_processing`.
pub fn parse_slots(v: &Value) -> Option<Slots> {
    let list = v.as_array()?;
    let total = u32::try_from(list.len()).ok()?;
    let busy = list
        .iter()
        .filter(|s| s.get("is_processing").and_then(Value::as_bool) == Some(true))
        .count();
    Some(Slots {
        total,
        busy: u32::try_from(busy).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/llama")
            .join(name);
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn parses_props_fixture() {
        let p = parse_props(&fixture("props.json"));
        assert_eq!(p.model.as_deref(), Some("qwen2.5-3b-instruct-q4_k_m"));
        assert_eq!(p.ctx_size, Some(4096));
        assert_eq!(p.slots_total, Some(2));
    }

    #[test]
    fn props_without_alias_falls_back_to_file_name() {
        let p = parse_props(&serde_json::json!({"model_path": "/sdcard/models/phi-3.gguf"}));
        assert_eq!(p.model.as_deref(), Some("phi-3"));
        assert_eq!(p.ctx_size, None);
    }

    #[test]
    fn parses_slots_fixture() {
        let s = parse_slots(&fixture("slots.json")).unwrap();
        assert_eq!(s, Slots { total: 2, busy: 1 });
    }

    #[test]
    fn slots_error_body_is_none() {
        // Con --no-slots llama-server responde un objeto de error, no un array.
        assert!(parse_slots(&fixture("slots_disabled.json")).is_none());
    }
}
