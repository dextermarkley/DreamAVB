//! JSON report accumulation for `--json <path>`.
//!
//! The file holds one top-level object. Each test appends a record under its
//! own key (an array), so repeated runs and `all` build one combined report.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Map, Value};

pub fn append<T: Serialize>(path: &Path, key: &str, record: &T) -> Result<()> {
    let mut root = match fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => match serde_json::from_str::<Value>(&s)
            .with_context(|| format!("parsing existing report {}", path.display()))?
        {
            Value::Object(m) => m,
            _ => Map::new(),
        },
        _ => Map::new(),
    };
    let value = serde_json::to_value(record)?;
    let slot = root
        .entry(key.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    match slot {
        Value::Array(a) => a.push(value),
        other => *other = Value::Array(vec![other.take(), value]),
    }
    let text = serde_json::to_string_pretty(&Value::Object(root))?;
    fs::write(path, text + "\n").with_context(|| format!("writing report {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_under_key() {
        // Scratch under the (gitignored) workspace target dir, never /tmp.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-scratch")
            .join(format!("avb-probe-report-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("r.json");
        let _ = fs::remove_file(&p);
        append(&p, "clock", &serde_json::json!({"ppm": 1.5})).unwrap();
        append(&p, "clock", &serde_json::json!({"ppm": 2.5})).unwrap();
        append(&p, "info", &serde_json::json!({"stage": 1})).unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["clock"].as_array().unwrap().len(), 2);
        assert_eq!(v["clock"][1]["ppm"], 2.5);
        assert_eq!(v["info"][0]["stage"], 1);
        fs::remove_dir_all(&dir).unwrap();
    }
}
