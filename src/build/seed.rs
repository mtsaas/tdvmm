//! __seed-build (runs inside `podman unshare`)

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::engine;
use super::ux::{capture, run};

#[derive(Serialize, Deserialize)]
pub(super) struct SeedSquash {
    pub(super) key: String,
    pub(super) local_tag: String,
    pub(super) tar: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub(super) struct SeedConfig {
    pub(super) store: PathBuf,
    pub(super) runroot: PathBuf,
    pub(super) conf: PathBuf,
    pub(super) plains: Vec<String>,
    pub(super) squash: Vec<SeedSquash>,
    pub(super) seedpins_out: PathBuf,
}

pub fn cmd_seed_build(config: &str) -> Result<i32, Box<dyn std::error::Error>> {
    let cfg: SeedConfig = serde_json::from_slice(&std::fs::read(config)?)?;
    let sp = |args: &[&str]| -> Command {
        let mut c = engine::scratch(&cfg.store, &cfg.runroot, &cfg.conf);
        c.args(args);
        c
    };
    for reff in &cfg.plains {
        run(&mut sp(&["pull", "-q", reff]), engine::OutputMode::Inherit)?;
    }
    for s in &cfg.squash {
        run(sp(&["load", "-q", "-i"]).arg(&s.tar), engine::OutputMode::Inherit)?;
    }
    // resolve the post-load SEED digest of each squashed/built image.
    let mut seedpins: HashMap<String, String> = HashMap::new();
    for s in &cfg.squash {
        let repo = s.local_tag.rsplit_once(':').map(|(r, _)| r).unwrap_or(&s.local_tag);
        let digests = capture(&mut sp(&["image", "inspect", &s.local_tag, "--format", "{{range .RepoDigests}}{{println .}}{{end}}"]))?;
        let pin = digests
            .lines()
            .find(|l| l.starts_with(&format!("{repo}@")))
            .map(|l| l.to_string())
            .unwrap_or_default();
        seedpins.insert(s.key.clone(), pin);
    }
    std::fs::write(&cfg.seedpins_out, serde_json::to_vec(&seedpins)?)?;
    Ok(0)
}
