//! The whitelist gate: parse an untrusted `compose.yml` and enforce the supported
//! subset, rejecting everything outside it with the loud `TDVMM_BAKE_REJECT:`
//! diagnostics (published ports warn and are stripped instead).
//!
//! [`validate`] returns the facts the bake needs next — the images to pull, the
//! host-side `build:` contexts, the relative binds to materialize, and any warnings.
//! Before inspecting the subset it walks the whole document once for shapes the lock
//! emitter cannot reproduce ([custom tags](reject_unrepresentable),
//! [scientific-notation floats](super::yaml_emitter::float_is_representable), and
//! non-string mapping keys) and rejects them, so no input that passes validation can
//! panic or silently diverge when [`emit_lock`](super::emit_lock) renders it.

use std::path::Path;

use serde_yaml::{Mapping, Value};

use super::error::{io, reject, ValidateError};
use super::yaml_emitter::float_is_representable;

/// A host-side `build:` context resolved by validation.
#[derive(Debug, Clone)]
pub struct BuildCtx {
    pub service: String,
    pub context: String,    // absolute
    pub dockerfile: String, // absolute
    pub image_tag: String,
    pub bases: Vec<String>,
}

/// A relative bind to materialize into the guest image.
#[derive(Debug, Clone)]
pub struct Bind {
    pub service: String,
    pub src: String, // absolute host path
    pub target: String,
    pub basename: String,
}

/// The validation result: the facts the bake needs next.
#[derive(Debug, Default)]
pub struct Validated {
    pub images: Vec<String>,
    pub builds: Vec<BuildCtx>,
    pub binds: Vec<Bind>,
    pub warnings: Vec<String>,
}

/// Enforce the supported subset. `compose_path` resolves relative bind/build paths
/// and checks they exist.
///
/// # Errors
///
/// Returns [`ValidateError::Reject`] for any compose outside the supported subset,
/// and [`ValidateError::Io`] if a referenced Dockerfile cannot be read.
pub fn validate(doc: &Value, compose_path: &Path) -> Result<Validated, ValidateError> {
    // Reject shapes the lock emitter cannot reproduce before looking at anything
    // else, so validation is the single gate that keeps them out of emit-lock.
    reject_unrepresentable(doc)?;

    let mut out = Validated::default();

    let services = match doc.get("services") {
        Some(Value::Mapping(m)) if !m.is_empty() => m,
        _ => return Err(reject(format!("{}: no services defined", compose_path.display()))),
    };

    reject_external_networks(doc)?;

    for (skey, scfg_v) in services {
        let Some(sname) = skey.as_str() else {
            return Err(reject("a service name is not a string"));
        };
        let scfg = match scfg_v {
            Value::Mapping(m) => m,
            _ => return Err(reject(format!("service '{sname}': not a mapping"))),
        };
        validate_service(sname, scfg, compose_path, &mut out)?;
    }

    Ok(out)
}

/// Reject a document containing a shape the lock emitter cannot reproduce: a custom
/// YAML tag (which has no plain scalar rendering), a float whose magnitude would
/// force scientific notation, or a non-string mapping key. Walks keys and values
/// recursively.
fn reject_unrepresentable(value: &Value) -> Result<(), ValidateError> {
    match value {
        Value::Tagged(t) => Err(reject(format!(
            "compose uses a custom YAML tag '{}'. Custom tags are outside the \
             supported subset and cannot be emitted to the lock; remove it.",
            t.tag
        ))),
        Value::Number(n) => {
            // Only floats can diverge; integers render identically in both emitters.
            let is_float = n.as_i64().is_none() && n.as_u64().is_none();
            if let Some(f) = n.as_f64() {
                if is_float && !float_is_representable(f) {
                    return Err(reject(format!(
                        "compose contains the float {f}, which YAML renders in \
                         scientific notation and cannot be reproduced byte-for-byte \
                         in the lock. Use an integer or a value in [0.0001, 1e16)."
                    )));
                }
            }
            Ok(())
        }
        Value::Sequence(items) => items.iter().try_for_each(reject_unrepresentable),
        Value::Mapping(m) => m.iter().try_for_each(|(k, v)| {
            if !matches!(k, Value::String(_)) {
                return Err(reject(
                    "compose has a mapping key that is not a string; only string keys \
                     are supported and a non-string key cannot be reproduced in the lock.",
                ));
            }
            reject_unrepresentable(k)?;
            reject_unrepresentable(v)
        }),
        _ => Ok(()),
    }
}

/// Reject any `networks:` entry declared `external:` — the closed-world guest cannot
/// join a pre-existing host network.
fn reject_external_networks(doc: &Value) -> Result<(), ValidateError> {
    let Some(Value::Mapping(nets)) = doc.get("networks") else {
        return Ok(());
    };
    for (nname, ncfg) in nets {
        let Value::Mapping(nm) = ncfg else { continue };
        let ext = nm.get(Value::String("external".into()));
        let is_ext = matches!(ext, Some(Value::Bool(true)))
            || matches!(ext, Some(Value::Mapping(_)))
            || matches!(ext, Some(Value::String(_)));
        if is_ext {
            let Some(nn) = nname.as_str() else {
                return Err(reject("a network name is not a string"));
            };
            return Err(reject(format!(
                "network '{nn}' is declared external:. The closed-world guest \
                 cannot join a pre-existing host network. Remove 'external: true' \
                 and let compose create a private network."
            )));
        }
    }
    Ok(())
}

/// Validate one service and record its image/build and binds into `out`. Enforces the
/// image-XOR-build rule, the closed-world bans (pull_policy: always, network_mode:
/// host, absolute binds), and warns on published ports.
fn validate_service(
    sname: &str,
    scfg: &Mapping,
    compose_path: &Path,
    out: &mut Validated,
) -> Result<(), ValidateError> {
    // image: (pull) XOR build: (host-side build at bake time)
    let image = scfg.get(Value::String("image".into())).and_then(|v| v.as_str());
    if let Some(build) = scfg.get(Value::String("build".into())) {
        out.builds.push(validate_build(build, sname, compose_path, scfg)?);
    } else {
        match image {
            None => {
                return Err(reject(format!(
                    "service '{sname}' has no image: and no build:. A service must \
                     reference a pulled image: or provide a build: context."
                )));
            }
            Some(img) => {
                if !out.images.iter().any(|i| i == img) {
                    out.images.push(img.to_string());
                }
            }
        }
    }

    // pull_policy: always
    if scfg.get(Value::String("pull_policy".into())).and_then(|v| v.as_str()) == Some("always") {
        return Err(reject(format!(
            "service '{sname}' sets pull_policy: always. The guest runs \
             --pull=never in a closed world; an always-pull can never be \
             satisfied offline. Remove pull_policy or set it to 'never'."
        )));
    }

    // network_mode: host
    if scfg.get(Value::String("network_mode".into())).and_then(|v| v.as_str()) == Some("host") {
        return Err(reject(format!(
            "service '{sname}' uses network_mode: host. The closed world \
             forbids host networking. Use a private compose network."
        )));
    }

    // ports: warn + strip
    if let Some(ports) = scfg.get(Value::String("ports".into())) {
        if !matches!(ports, Value::Null) {
            let ports_disp = flow_repr(ports);
            out.warnings.push(format!(
                "service '{sname}': published ports {ports_disp} STRIPPED \
                 (closed world has no host to publish to)."
            ));
        }
    }

    // volumes: classify binds
    if let Some(Value::Sequence(vols)) = scfg.get(Value::String("volumes".into())) {
        for entry in vols {
            if let Some(bind) = classify_bind(sname, entry, compose_path)? {
                out.binds.push(bind);
            }
        }
    }

    Ok(())
}

/// Classify one `volumes:` entry: `Ok(Some(bind))` for a relative bind to
/// materialize, `Ok(None)` for a named/anonymous volume left as-is, or a reject for
/// an absolute host bind or a missing source.
fn classify_bind(
    sname: &str,
    entry: &Value,
    compose_path: &Path,
) -> Result<Option<Bind>, ValidateError> {
    let Some((src, target, _mode)) = split_bind(entry) else {
        return Ok(None); // named/anonymous volume
    };
    if src.starts_with('/') || src.starts_with('~') {
        return Err(reject(format!(
            "service '{sname}' binds absolute host path '{src}'. The \
             closed-world guest has no host filesystem. Only RELATIVE \
             binds (materialized into the guest image) are supported."
        )));
    }
    let base = compose_path.parent().unwrap_or_else(|| Path::new("."));
    let abssrc = normpath(&base.join(&src));
    if !Path::new(&abssrc).exists() {
        return Err(reject(format!(
            "service '{sname}' bind source '{src}' does not exist next to \
             the compose file ({abssrc})."
        )));
    }
    let basename = Path::new(src.trim_end_matches('/'))
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Some(Bind { service: sname.to_string(), src: abssrc, target, basename }))
}

/// (src, target, mode) for a bind entry, or `None` for a named/anonymous volume
/// (kept as-is). Long-form dict binds are handled.
pub(super) fn split_bind(entry: &Value) -> Option<(String, String, String)> {
    match entry {
        Value::String(s) => {
            let parts: Vec<&str> = s.split(':').collect();
            if parts.len() == 1 {
                return None; // anonymous volume
            }
            let src = parts[0].to_string();
            let target = parts[1].to_string();
            let mode = if parts.len() > 2 { parts[2].to_string() } else { "rw".to_string() };
            let looks_like_path =
                src.starts_with('.') || src.starts_with('/') || src.starts_with('~') || src.contains('/');
            if !looks_like_path {
                return None; // named volume
            }
            Some((src, target, mode))
        }
        Value::Mapping(m) => {
            let vtype = m
                .get(Value::String("type".into()))
                .and_then(|v| v.as_str())
                .unwrap_or("volume");
            if vtype == "bind" {
                let src = m.get(Value::String("source".into())).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let target = m.get(Value::String("target".into())).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let ro = m.get(Value::String("read_only".into())).and_then(|v| v.as_bool()).unwrap_or(false);
                let mode = if ro { "ro" } else { "rw" }.to_string();
                Some((src, target, mode))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The image tag a `build:` service resolves to: its explicit `image:` if any, else a
/// deterministic `localhost/tdvmm-build-<safe-name>:baked`.
pub(super) fn build_output_tag(sname: &str, scfg: &Mapping) -> String {
    if let Some(img) = scfg.get(Value::String("image".into())).and_then(|v| v.as_str()) {
        return img.to_string();
    }
    let safe: String = sname
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect();
    format!("localhost/tdvmm-build-{safe}:baked")
}

/// External FROM bases in a Dockerfile (stage aliases tracked).
fn parse_dockerfile_froms(dockerfile_path: &Path) -> Result<Vec<String>, ValidateError> {
    let text = std::fs::read_to_string(dockerfile_path)
        .map_err(|e| io(format!("reading {}", dockerfile_path.display()), e))?;
    let mut stages: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut bases = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.len() >= 2 && toks[0].eq_ignore_ascii_case("FROM") {
            let r = toks[1];
            if toks.len() >= 4 && toks[2].eq_ignore_ascii_case("AS") {
                stages.insert(toks[3].to_string());
            }
            if !stages.contains(r) {
                bases.push(r.to_string());
            }
        }
    }
    Ok(bases)
}

/// Validate a service's `build:` context: only `context`/`dockerfile` keys, a relative
/// context directory that exists, a Dockerfile with at least one FROM, and every
/// external FROM digest-pinned by `@sha256:`.
fn validate_build(
    build: &Value,
    sname: &str,
    compose_path: &Path,
    scfg: &Mapping,
) -> Result<BuildCtx, ValidateError> {
    let (context, dockerfile) = match build {
        Value::String(s) => (s.clone(), "Dockerfile".to_string()),
        Value::Mapping(m) => {
            for k in m.keys() {
                if let Some(ks) = k.as_str() {
                    if ks != "context" && ks != "dockerfile" {
                        return Err(reject(format!(
                            "service '{sname}' build: uses unsupported key(s) [\"{ks}\"]. \
                             Only 'context' and 'dockerfile' are supported (build args/target \
                             would compromise closed-world reproducibility). Remove them."
                        )));
                    }
                }
            }
            let context = m.get(Value::String("context".into())).and_then(|v| v.as_str()).unwrap_or(".").to_string();
            let dockerfile = m.get(Value::String("dockerfile".into())).and_then(|v| v.as_str()).unwrap_or("Dockerfile").to_string();
            (context, dockerfile)
        }
        _ => return Err(reject(format!("service '{sname}' build: must be a path or a mapping."))),
    };

    if context.starts_with('/') || context.starts_with('~') {
        return Err(reject(format!(
            "service '{sname}' build context '{context}' is absolute. Only a \
             RELATIVE build context (next to the compose file) is supported, so \
             the bake is self-contained and closed-world."
        )));
    }
    let base = compose_path.parent().unwrap_or_else(|| Path::new("."));
    let abs_ctx = normpath(&base.join(&context));
    if !Path::new(&abs_ctx).is_dir() {
        return Err(reject(format!(
            "service '{sname}' build context '{context}' is not a directory ({abs_ctx})."
        )));
    }
    let abs_df = normpath(&Path::new(&abs_ctx).join(&dockerfile));
    if !Path::new(&abs_df).is_file() {
        return Err(reject(format!(
            "service '{sname}' build dockerfile '{dockerfile}' not found in the context ({abs_df})."
        )));
    }
    let bases = parse_dockerfile_froms(Path::new(&abs_df))?;
    if bases.is_empty() {
        return Err(reject(format!("service '{sname}' build {dockerfile} has no FROM instruction.")));
    }
    for b in &bases {
        if !b.contains("@sha256:") {
            return Err(reject(format!(
                "service '{sname}' build base image '{b}' is NOT digest-pinned. \
                 Every external FROM must be pinned by @sha256:<digest> so the \
                 host-side build is reproducible + closed-world. Pin it."
            )));
        }
    }
    Ok(BuildCtx {
        service: sname.to_string(),
        context: abs_ctx,
        dockerfile: abs_df,
        image_tag: build_output_tag(sname, scfg),
        bases,
    })
}

/// Normalize a path like Python `os.path.normpath` (lexical; no symlink resolution)
/// and return an absolute string.
fn normpath(p: &Path) -> String {
    use std::path::Component;
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    let mut is_abs = false;
    for comp in p.components() {
        match comp {
            Component::RootDir => {
                is_abs = true;
                out.clear();
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last().map(|s| s.to_str()), Some(Some("..")) | None) && !is_abs {
                    out.push("..".into());
                } else if !out.is_empty() {
                    out.pop();
                }
            }
            Component::Normal(s) => out.push(s.to_os_string()),
            Component::Prefix(_) => {}
        }
    }
    let joined = out
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    if is_abs {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// A Python-`repr`-ish rendering of a ports value, for the WARN message (only its
/// substring "published ports" is asserted; this just keeps the text informative).
fn flow_repr(v: &Value) -> String {
    match v {
        Value::Sequence(items) => {
            let inner: Vec<String> = items.iter().map(flow_repr).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::String(s) => format!("'{s}'"),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => if *b { "True".into() } else { "False".into() },
        Value::Null => "None".into(),
        Value::Mapping(_) => "{...}".into(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Value {
        serde_yaml::from_str(src).expect("test fixture parses")
    }

    /// Validate a `testdata/stacks/rejects/*.yml` corpus fixture (cwd is the repo root).
    fn validate_fixture(file: &str) -> Result<Validated, ValidateError> {
        let path = format!("testdata/stacks/rejects/{file}");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let doc = parse(&raw);
        validate(&doc, Path::new(&path))
    }

    fn assert_reject(res: Result<Validated, ValidateError>, needle: &str) {
        match res {
            Err(ValidateError::Reject(msg)) => assert!(
                msg.to_lowercase().contains(&needle.to_lowercase()),
                "reject message {msg:?} does not contain {needle:?}"
            ),
            other => panic!("expected a Reject containing {needle:?}, got {other:?}"),
        }
    }

    #[test]
    fn corpus_rejects_have_their_diagnostics() {
        assert_reject(validate_fixture("absbind.yml"), "absolute host path");
        assert_reject(validate_fixture("absbind_rw.yml"), "absolute host path");
        assert_reject(validate_fixture("extnet.yml"), "external");
        assert_reject(validate_fixture("pullalways.yml"), "pull_policy: always");
        assert_reject(validate_fixture("buildunpinned.yml"), "NOT digest-pinned");
    }

    #[test]
    fn published_ports_warn_and_do_not_reject() {
        let out = validate_fixture("ports.yml").expect("ports warn, not reject");
        assert!(
            out.warnings.iter().any(|w| w.contains("published ports")),
            "expected a published-ports warning, got {:?}",
            out.warnings
        );
    }

    #[test]
    fn no_services_is_rejected() {
        assert_reject(validate(&parse("name: x\n"), Path::new("compose.yml")), "no services defined");
    }

    #[test]
    fn service_without_image_or_build_is_rejected() {
        let doc = parse("services:\n  app:\n    command: [\"true\"]\n");
        assert_reject(validate(&doc, Path::new("compose.yml")), "no image: and no build:");
    }

    #[test]
    fn network_mode_host_is_rejected() {
        let doc = parse(
            "services:\n  app:\n    image: busybox\n    network_mode: host\n",
        );
        assert_reject(validate(&doc, Path::new("compose.yml")), "network_mode: host");
    }

    /// The panic fix: a custom-tagged scalar in an unvalidated field is a clean
    /// reject, not a fall-through to the emitter's old `expect`.
    #[test]
    fn custom_tagged_scalar_is_rejected() {
        let doc = parse(
            "services:\n  app:\n    image: busybox\n    environment:\n      - !secret hunter2\n",
        );
        assert_reject(validate(&doc, Path::new("compose.yml")), "custom YAML tag");
    }

    /// Floats that YAML would render in scientific notation are rejected rather than
    /// emitted as a lock that disagrees with the Python producer.
    #[test]
    fn scientific_notation_floats_are_rejected() {
        for lit in ["2.5e16", "1.0e-5"] {
            let doc = parse(&format!(
                "services:\n  app:\n    image: busybox\n    environment:\n      X: {lit}\n"
            ));
            assert_reject(validate(&doc, Path::new("compose.yml")), "scientific notation");
        }
    }

    /// A finite float in the fixed-notation range is accepted (no divergence).
    #[test]
    fn in_range_float_is_accepted() {
        let doc = parse(
            "services:\n  app:\n    image: busybox\n    environment:\n      X: 100.5\n",
        );
        assert!(validate(&doc, Path::new("compose.yml")).is_ok());
    }

    /// A non-string mapping key is untrusted input and must be rejected, not coerced
    /// to an empty string.
    #[test]
    fn non_string_service_key_is_rejected() {
        let doc = parse("services:\n  1: { image: busybox }\n");
        assert_reject(validate(&doc, Path::new("compose.yml")), "not a string");
    }

    /// A non-string key nested anywhere in the document is rejected too, not only
    /// top-level service/network names — otherwise the emitter would render it
    /// divergently from PyYAML (a single-quoted key where PyYAML emits it plain).
    #[test]
    fn nested_non_string_key_is_rejected() {
        let doc = parse("services:\n  web:\n    image: busybox\n    environment:\n      1: value\n");
        assert_reject(validate(&doc, Path::new("compose.yml")), "not a string");
    }
}
