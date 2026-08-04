//! The JSONL run-log writer: one line per event, `{schema, ts_vtsc, t_s,
//! wall_ms, type, ...}`. Owns the line format — a stable, shared contract — and
//! its sorted-key serialization (serde_json's default `Map`), so callers only
//! hand it a payload `Value`.

use std::fs::File;
use std::io::Write;
use std::time::Instant;

use serde_json::{json, Value};

use crate::vtsc::TscFrequency;

use super::{round3, SCHEMA_VERSION};

pub(super) struct Logger {
    file: File,
    freq: TscFrequency,
    t0: u64,
    start: Instant,
}

impl Logger {
    /// # Errors
    /// Returns the `io::Error` if the run-log file cannot be created.
    pub(super) fn new(path: &str, freq: TscFrequency) -> std::io::Result<Self> {
        let file = File::create(path)?;
        Ok(Self {
            file,
            freq,
            t0: 0,
            start: Instant::now(),
        })
    }
    pub(super) fn set_t0(&mut self, t0: u64) {
        self.t0 = t0;
    }
    pub(super) fn event(&mut self, now: u64, typ: &str, mut payload: Value) {
        let obj = payload.as_object_mut();
        let t_s = now.saturating_sub(self.t0) as f64 / self.freq.hz() as f64;
        let mut line = serde_json::Map::new();
        line.insert("schema".into(), json!(SCHEMA_VERSION));
        line.insert("ts_vtsc".into(), json!(now));
        line.insert("t_s".into(), json!(round3(t_s)));
        line.insert("wall_ms".into(), json!(self.start.elapsed().as_millis() as u64));
        line.insert("type".into(), json!(typ));
        if let Some(o) = obj {
            for (k, v) in o.iter() {
                line.insert(k.clone(), v.clone());
            }
        }
        let mut bytes = serde_json::to_vec(&Value::Object(line)).unwrap_or_default();
        bytes.push(b'\n');
        let _ = self.file.write_all(&bytes);
        let _ = self.file.flush();
    }
}
