//! The Rust compose pipeline for `tdvmm build` (OP-1b) — a faithful in-binary port
//! of the retired `guest/bake_compose.py`.
//!
//! Two jobs, matching the Python original exactly:
//!
//!   * **validate** — parse a `compose.yml`, enforce the SUPPORTED SUBSET, and
//!     reject everything outside it with the same loud `TDVMM_BAKE_REJECT:`
//!     diagnostics. Returns the images to bake, the host-side `build:` contexts,
//!     the relative binds to materialize, and any warnings (stripped ports).
//!
//!   * **emit-lock** — given the resolved image digests + the in-guest bind base
//!     + the pinned project name, write the deterministic `compose.lock.yml` (the
//!     ONLY compose file the guest sees) and the bind copy-manifest.
//!
//! ## Byte-identical YAML — a PyYAML `safe_dump` port
//!
//! The OP-1b gate is a **byte-identical** `.tdvmm` versus the old script producer,
//! and `compose.lock.yml` is embedded in BOTH the initramfs and the `.tdvmm`. The
//! Python emitted the lock with `yaml.safe_dump(doc, sort_keys=True,
//! default_flow_style=False)`. libyaml-based crates (serde_yaml) do NOT reproduce
//! PyYAML's exact wrapping/quoting, so [`emit_yaml`] is a hand port of PyYAML's
//! emitter (`analyze_scalar` / `choose_scalar_style` / `write_plain` /
//! `write_single_quoted` / `write_double_quoted` / block layout) with the same
//! `best_indent=2`, `best_width=80`, `allow_unicode=False` defaults — reproducing
//! its output byte-for-byte. We parse with `serde_yaml` (mature) but emit
//! ourselves; the round-trip tests pin fidelity against the committed corpus locks.

use std::collections::HashMap;
use std::path::Path;

use serde_yaml::Value;

// ============================================================================
// Validation (port of bake_compose.py `validate`) — the loud, static subset gate
// ============================================================================

pub const REJECT: &str = "TDVMM_BAKE_REJECT";
pub const WARN: &str = "TDVMM_BAKE_WARN";

/// A validation failure, carrying the exit code the Python used: a REJECT
/// (out-of-subset compose) is exit 3; an ERROR (bad file / internal) is exit 2.
#[derive(Debug)]
pub struct ValidateError {
    pub exit_code: i32,
    pub message: String,
}

fn reject(msg: impl Into<String>) -> ValidateError {
    ValidateError { exit_code: 3, message: msg.into() }
}
fn error(msg: impl Into<String>) -> ValidateError {
    ValidateError { exit_code: 2, message: msg.into() }
}

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
    #[allow(dead_code)]
    pub rel: String,
    pub target: String,
    #[allow(dead_code)]
    pub mode: String, // "ro" | "rw" (mirrors the Python bind dict; lock re-derives it)
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

/// (src, target, mode) for a bind entry, or None for a NAMED/anonymous volume
/// (kept as-is). Long-form dict binds handled. Port of `split_bind`.
fn split_bind(entry: &Value) -> Option<(String, String, String)> {
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

/// The image tag a `build:` service resolves to. Port of `build_output_tag`.
fn build_output_tag(sname: &str, scfg: &serde_yaml::Mapping) -> String {
    if let Some(img) = scfg.get(Value::String("image".into())).and_then(|v| v.as_str()) {
        return img.to_string();
    }
    let safe: String = sname
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect();
    format!("localhost/tdvmm-build-{safe}:baked")
}

/// External FROM bases in a Dockerfile (stage aliases tracked). Port of
/// `parse_dockerfile_froms`.
fn parse_dockerfile_froms(dockerfile_path: &Path) -> Result<Vec<String>, ValidateError> {
    let text = std::fs::read_to_string(dockerfile_path)
        .map_err(|e| error(format!("reading {}: {e}", dockerfile_path.display())))?;
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

/// Validate a service's `build:` context. Port of `validate_build`.
fn validate_build(
    build: &Value,
    sname: &str,
    compose_path: &Path,
    scfg: &serde_yaml::Mapping,
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

/// Normalize a path like Python `os.path.normpath` (lexical; no symlink
/// resolution) and return an absolute string.
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

/// Enforce the supported subset. Port of `validate`. `compose_path` is used to
/// resolve relative bind/build paths and check they exist.
pub fn validate(doc: &Value, compose_path: &Path) -> Result<Validated, ValidateError> {
    let mut out = Validated::default();

    let services = match doc.get("services") {
        Some(Value::Mapping(m)) if !m.is_empty() => m,
        _ => return Err(reject(format!("{}: no services defined", compose_path.display()))),
    };

    // networks: reject external.
    if let Some(Value::Mapping(nets)) = doc.get("networks") {
        for (nname, ncfg) in nets {
            if let Value::Mapping(nm) = ncfg {
                let ext = nm.get(Value::String("external".into()));
                let is_ext = matches!(ext, Some(Value::Bool(true)))
                    || matches!(ext, Some(Value::Mapping(_)))
                    || matches!(ext, Some(Value::String(_)));
                if is_ext {
                    let nn = nname.as_str().unwrap_or("");
                    return Err(reject(format!(
                        "network '{nn}' is declared external:. The closed-world guest \
                         cannot join a pre-existing host network. Remove 'external: true' \
                         and let compose create a private network."
                    )));
                }
            }
        }
    }

    for (skey, scfg_v) in services {
        let sname = skey.as_str().unwrap_or("");
        let scfg = match scfg_v {
            Value::Mapping(m) => m,
            _ => return Err(reject(format!("service '{sname}': not a mapping"))),
        };

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
                let Some((src, target, mode)) = split_bind(entry) else {
                    continue; // named/anonymous volume
                };
                if src.starts_with('/') || src.starts_with('~') {
                    return Err(reject(format!(
                        "service '{sname}' binds absolute host path '{src}'. The \
                         closed-world guest has no host filesystem. Only RELATIVE \
                         binds (materialized into the guest image) are supported."
                    )));
                }
                let norm_mode = if mode.split(',').any(|m| m == "ro") { "ro" } else { "rw" }.to_string();
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
                out.binds.push(Bind {
                    service: sname.to_string(),
                    src: abssrc,
                    rel: src.clone(),
                    target,
                    mode: norm_mode,
                    basename,
                });
            }
        }
    }

    Ok(out)
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

// ============================================================================
// emit-lock (port of bake_compose.py `emit-lock`) — the deterministic lockfile
// ============================================================================

/// The generated header PyYAML output was prefixed with (kept byte-identical to
/// the retired `bake_compose.py` so the `.tdvmm` gate holds).
pub const LOCK_HEADER: &str = "\
# GENERATED by bake_compose.py -- do NOT edit. The only compose
# file the guest ever sees. Images pinned by digest; ports stripped;
# relative RO binds materialized to in-guest paths; project pinned.
";

/// Result of `emit-lock`: the lockfile bytes and the bind copy-manifest (each
/// entry: (host-src-abs, in-guest-relative-dest)).
pub struct LockOutput {
    pub lock_yaml: Vec<u8>,
    pub bind_manifest: Vec<(String, String)>,
}

/// The guest-side event-bridge FIFO bind path (schema 3): created on the /run
/// tmpfs at boot and bind-mounted read-write into every service so workloads can
/// write assertion events the agent forwards over ttyS1. Emitted verbatim (never
/// rewritten like a relative bind); source of truth is [`tdvmm_proto::EVENT_FIFO_PATH`].
pub const EVENT_FIFO: &str = tdvmm_proto::EVENT_FIFO_PATH;

/// Transform the parsed compose `doc` into the deterministic lockfile. Port of
/// `cmd_emit_lock`. `digests` maps an original image ref (pulls) or a build
/// output tag (builds) to its pinned `repo@sha256`.
pub fn emit_lock(
    doc: &Value,
    compose_path: &Path,
    digests: &HashMap<String, String>,
    binds_base: &str,
    project: &str,
) -> Result<LockOutput, ValidateError> {
    // Re-validate first (same as Python) so a lock is never emitted for a reject.
    let validated = validate(doc, compose_path)?;

    // bind copy-manifest + in-guest dest path per (service,target).
    let mut bind_manifest = Vec::new();
    let mut dest_of: HashMap<(String, String), String> = HashMap::new();
    for b in &validated.binds {
        let dest = format!("{}/{}/{}", binds_base, b.service, b.basename);
        dest_of.insert((b.service.clone(), b.target.clone()), dest.clone());
        bind_manifest.push((b.src.clone(), format!("{}/{}", b.service, b.basename)));
    }

    let mut doc = doc.clone();
    let m = doc.as_mapping_mut().ok_or_else(|| reject("top level is not a mapping"))?;
    m.insert(Value::String("name".into()), Value::String(project.to_string()));

    let services = m
        .get_mut(Value::String("services".into()))
        .and_then(|v| v.as_mapping_mut())
        .ok_or_else(|| error("services missing"))?;

    // Iterate services in document order (matches Python dict iteration).
    let snames: Vec<String> = services.keys().filter_map(|k| k.as_str().map(String::from)).collect();
    for sname in snames {
        let scfg = services
            .get_mut(Value::String(sname.clone()))
            .and_then(|v| v.as_mapping_mut())
            .ok_or_else(|| error(format!("service '{sname}' not a mapping")))?;

        if scfg.contains_key(Value::String("build".into())) {
            let tag = build_output_tag(&sname, scfg);
            scfg.remove(Value::String("build".into()));
            match digests.get(&tag) {
                Some(pin) => {
                    scfg.insert(Value::String("image".into()), Value::String(pin.clone()));
                }
                None => {
                    return Err(error(format!(
                        "service '{sname}' build output '{tag}' was not baked/pinned \
                         (no digest supplied to emit-lock)."
                    )));
                }
            }
        } else if let Some(img) = scfg.get(Value::String("image".into())).and_then(|v| v.as_str()).map(String::from) {
            if let Some(pin) = digests.get(&img) {
                scfg.insert(Value::String("image".into()), Value::String(pin.clone()));
            }
        }

        scfg.remove(Value::String("ports".into()));
        scfg.remove(Value::String("pull_policy".into()));

        // Rewrite relative binds (ro OR rw) to in-guest absolute paths, then inject
        // the event-bridge FIFO bind (schema 3) so every workload can write assertion
        // events that the agent forwards over ttyS1. The FIFO file is created on the
        // guest /run tmpfs at boot; binding the FILE (not the dir) means no container
        // can unlink or replace the shared inode. Absolute path ⇒ emitted verbatim.
        let existing = match scfg.get(Value::String("volumes".into())).cloned() {
            Some(Value::Sequence(vols)) => vols,
            _ => Vec::new(),
        };
        let mut newvols: Vec<Value> = Vec::new();
        for entry in &existing {
            if let Some((src, target, mode)) = split_bind(entry) {
                if !(src.starts_with('/') || src.starts_with('~')) {
                    if let Some(dest) = dest_of.get(&(sname.clone(), target.clone())) {
                        let norm_mode = if mode.split(',').any(|x| x == "ro") { "ro" } else { "rw" };
                        newvols.push(Value::String(format!("{dest}:{target}:{norm_mode}")));
                        continue;
                    }
                }
            }
            newvols.push(entry.clone());
        }
        newvols.push(Value::String(format!("{EVENT_FIFO}:{EVENT_FIFO}:rw")));
        scfg.insert(Value::String("volumes".into()), Value::Sequence(newvols));
    }

    let mut lock_yaml = LOCK_HEADER.as_bytes().to_vec();
    lock_yaml.extend_from_slice(&emit_yaml(&doc));

    Ok(LockOutput { lock_yaml, bind_manifest })
}

// ============================================================================
// PyYAML-faithful YAML emitter (safe_dump: sort_keys=True, default_flow_style=False)
// ============================================================================

const BEST_INDENT: i64 = 2;
const BEST_WIDTH: i64 = 80;

/// The scalar's YAML 1.1 core tag as PyYAML's default resolver would detect it
/// from the *plain* rendering — this drives whether a value can stay unquoted.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum Tag {
    Str,
    Int,
    Float,
    Bool,
    Null,
    Other, // merge/value/yaml — irrelevant here, never our target tag
}

/// PyYAML's default implicit resolver, restricted to what a plain scalar can
/// resolve to. Mirrors `Resolver.yaml_implicit_resolvers` (YAML 1.1 schema, still
/// shipped by PyYAML 6): bool includes yes/no/on/off and case variants.
fn resolve_plain(value: &str) -> Tag {
    if value.is_empty() {
        return Tag::Null; // the empty branch of the null pattern
    }
    let b0 = value.as_bytes()[0];
    // The resolver only consults patterns whose first-char index contains b0;
    // checking all is equivalent since the regexes are anchored.
    // null: ~ | null | Null | NULL | (empty handled above)
    match value {
        "~" | "null" | "Null" | "NULL" => return Tag::Null,
        _ => {}
    }
    match value {
        "yes" | "Yes" | "YES" | "no" | "No" | "NO" | "true" | "True" | "TRUE"
        | "false" | "False" | "FALSE" | "on" | "On" | "ON" | "off" | "Off" | "OFF" => {
            return Tag::Bool
        }
        _ => {}
    }
    // Fast reject: int/float/timestamp all start with a digit, sign, or dot.
    let numeric_lead = b0.is_ascii_digit() || b0 == b'-' || b0 == b'+' || b0 == b'.';
    if numeric_lead {
        if int_re(value) {
            return Tag::Int;
        }
        if float_re(value) {
            return Tag::Float;
        }
        if timestamp_re(value) {
            return Tag::Other; // timestamp tag; != str, forces quoting
        }
    }
    Tag::Str
}

/// `^(?:[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+
///    |[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+)$`
fn int_re(s: &str) -> bool {
    let t = s.strip_prefix(['-', '+']).unwrap_or(s);
    if t.is_empty() {
        return false;
    }
    // 0b binary
    if let Some(r) = t.strip_prefix("0b") {
        return !r.is_empty() && r.bytes().all(|c| c == b'0' || c == b'1' || c == b'_');
    }
    // 0x hex
    if let Some(r) = t.strip_prefix("0x") {
        return !r.is_empty() && r.bytes().all(|c| c.is_ascii_hexdigit() || c == b'_');
    }
    // sexagesimal int: [1-9][0-9_]*(:[0-5]?[0-9])+
    if t.contains(':') {
        let mut parts = t.split(':');
        let first = parts.next().unwrap();
        if !(first.as_bytes()[0].is_ascii_digit()
            && first.as_bytes()[0] != b'0'
            && first.bytes().all(|c| c.is_ascii_digit() || c == b'_'))
        {
            return false;
        }
        let mut any = false;
        for p in parts {
            any = true;
            if p.is_empty() || p.len() > 2 || !p.bytes().all(|c| c.is_ascii_digit()) {
                return false;
            }
            if p.len() == 2 && !(b'0'..=b'5').contains(&p.as_bytes()[0]) {
                return false;
            }
        }
        return any;
    }
    // octal: 0[0-7_]+
    if t.starts_with('0') && t.len() > 1 {
        return t[1..].bytes().all(|c| (b'0'..=b'7').contains(&c) || c == b'_');
    }
    // plain: 0 | [1-9][0-9_]*
    if t == "0" {
        return true;
    }
    let tb = t.as_bytes();
    (b'1'..=b'9').contains(&tb[0]) && tb.iter().all(|&c| c.is_ascii_digit() || c == b'_')
}

/// A pragmatic port of PyYAML's float implicit pattern (enough for the subset —
/// any value that looks floaty must be quoted if it is meant as a string).
fn float_re(s: &str) -> bool {
    let t = s.strip_prefix(['-', '+']).unwrap_or(s);
    match t {
        ".inf" | ".Inf" | ".INF" => return true,
        _ => {}
    }
    match s {
        ".nan" | ".NaN" | ".NAN" => return true,
        _ => {}
    }
    // [0-9][0-9_]*\.[0-9_]*(e...)? | \.[0-9][0-9_]*(e...)?  (+ sexagesimal float)
    if !t.contains('.') {
        return false;
    }
    // Split off an exponent if present.
    let (mantissa, exp_ok) = match t.split_once(['e', 'E']) {
        Some((m, e)) => {
            let e = e.strip_prefix(['-', '+']).unwrap_or("");
            (m, !e.is_empty() && e.bytes().all(|c| c.is_ascii_digit()))
        }
        None => (t, true),
    };
    if !exp_ok {
        return false;
    }
    let Some((intp, frac)) = mantissa.split_once('.') else {
        return false;
    };
    let int_ok = !intp.is_empty()
        && intp.as_bytes()[0].is_ascii_digit()
        && intp.bytes().all(|c| c.is_ascii_digit() || c == b'_');
    let lead_dot_ok = intp.is_empty() // ".5"
        && !frac.is_empty()
        && frac.as_bytes()[0].is_ascii_digit();
    let frac_ok = frac.bytes().all(|c| c.is_ascii_digit() || c == b'_');
    (int_ok && frac_ok) || lead_dot_ok
}

/// YYYY-MM-DD date / full timestamp lead (only needs to be conservative).
fn timestamp_re(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 8
        && b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
        && b[4] == b'-'
}

/// Result of `analyze_scalar` — which styles are legal for a given string. Some
/// fields are unused by our block-only, safe_dump emitter but kept for a faithful
/// 1:1 port of PyYAML's `analyze_scalar`.
#[allow(dead_code)]
struct Analysis {
    empty: bool,
    multiline: bool,
    allow_flow_plain: bool,
    allow_block_plain: bool,
    allow_single_quoted: bool,
    allow_double_quoted: bool,
    allow_block: bool,
}

/// A direct port of `Emitter.analyze_scalar` (allow_unicode=False).
fn analyze_scalar(scalar: &str) -> Analysis {
    if scalar.is_empty() {
        return Analysis {
            empty: true,
            multiline: false,
            allow_flow_plain: false,
            allow_block_plain: true,
            allow_single_quoted: true,
            allow_double_quoted: true,
            allow_block: false,
        };
    }
    let chars: Vec<char> = scalar.chars().collect();
    let n = chars.len();

    let mut block_indicators = false;
    let mut flow_indicators = false;
    let mut line_breaks = false;
    let mut special_characters = false;

    let mut leading_space = false;
    let mut leading_break = false;
    let mut trailing_space = false;
    let mut trailing_break = false;
    let mut break_space = false;
    let mut space_break = false;

    if scalar.starts_with("---") || scalar.starts_with("...") {
        block_indicators = true;
        flow_indicators = true;
    }

    let is_ws = |c: char| matches!(c, '\0' | ' ' | '\t' | '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}');
    let is_break = |c: char| matches!(c, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}');

    let mut preceded_by_whitespace = true;
    let mut followed_by_whitespace = n == 1 || is_ws(chars[1]);
    let mut previous_space = false;
    let mut previous_break = false;

    let mut index = 0usize;
    while index < n {
        let ch = chars[index];
        if index == 0 {
            if matches!(ch, '#' | ',' | '[' | ']' | '{' | '}' | '&' | '*' | '!' | '|' | '>' | '\'' | '"' | '%' | '@' | '`') {
                flow_indicators = true;
                block_indicators = true;
            }
            if matches!(ch, '?' | ':') {
                flow_indicators = true;
                if followed_by_whitespace {
                    block_indicators = true;
                }
            }
            if ch == '-' && followed_by_whitespace {
                flow_indicators = true;
                block_indicators = true;
            }
        } else {
            if matches!(ch, ',' | '?' | '[' | ']' | '{' | '}') {
                flow_indicators = true;
            }
            if ch == ':' {
                flow_indicators = true;
                if followed_by_whitespace {
                    block_indicators = true;
                }
            }
            if ch == '#' && preceded_by_whitespace {
                flow_indicators = true;
                block_indicators = true;
            }
        }

        if is_break(ch) {
            line_breaks = true;
        }
        if !(ch == '\n' || ('\x20'..='\x7e').contains(&ch)) {
            // allow_unicode = False: any non-printable-ASCII (besides '\n') is special.
            special_characters = true;
        }

        if ch == ' ' {
            if index == 0 {
                leading_space = true;
            }
            if index == n - 1 {
                trailing_space = true;
            }
            if previous_break {
                break_space = true;
            }
            previous_space = true;
            previous_break = false;
        } else if is_break(ch) {
            if index == 0 {
                leading_break = true;
            }
            if index == n - 1 {
                trailing_break = true;
            }
            if previous_space {
                space_break = true;
            }
            previous_space = false;
            previous_break = true;
        } else {
            previous_space = false;
            previous_break = false;
        }

        index += 1;
        preceded_by_whitespace = is_ws(ch);
        followed_by_whitespace = index + 1 >= n || is_ws(chars[index + 1]);
    }

    let mut allow_flow_plain = true;
    let mut allow_block_plain = true;
    let mut allow_single_quoted = true;
    let allow_double_quoted = true;
    let mut allow_block = true;

    if leading_space || leading_break || trailing_space || trailing_break {
        allow_flow_plain = false;
        allow_block_plain = false;
    }
    if trailing_space {
        allow_block = false;
    }
    if break_space {
        allow_flow_plain = false;
        allow_block_plain = false;
        allow_single_quoted = false;
    }
    if space_break || special_characters {
        allow_flow_plain = false;
        allow_block_plain = false;
        allow_single_quoted = false;
        allow_block = false;
    }
    if line_breaks {
        allow_flow_plain = false;
        allow_block_plain = false;
    }
    if flow_indicators {
        allow_flow_plain = false;
    }
    if block_indicators {
        allow_block_plain = false;
    }

    Analysis {
        empty: false,
        multiline: line_breaks,
        allow_flow_plain,
        allow_block_plain,
        allow_single_quoted,
        allow_double_quoted,
        allow_block,
    }
}

const ESCAPE_REPLACEMENTS: &[(char, char)] = &[
    ('\0', '0'),
    ('\x07', 'a'),
    ('\x08', 'b'),
    ('\t', 't'),
    ('\n', 'n'),
    ('\x0b', 'v'),
    ('\x0c', 'f'),
    ('\r', 'r'),
    ('\x1b', 'e'),
    ('"', '"'),
    ('\\', '\\'),
    ('\u{85}', 'N'),
    ('\u{a0}', '_'),
    ('\u{2028}', 'L'),
    ('\u{2029}', 'P'),
];

/// The emitter state machine (the subset of `yaml.emitter.Emitter` we need).
struct Emitter {
    out: Vec<u8>,
    column: i64,
    indent: Option<i64>,
    indents: Vec<Option<i64>>,
    whitespace: bool,
    indention: bool,
    open_ended: bool,
}

impl Emitter {
    fn new() -> Emitter {
        Emitter {
            out: Vec::new(),
            column: 0,
            indent: None,
            indents: Vec::new(),
            whitespace: true,
            indention: true,
            open_ended: false,
        }
    }

    fn write(&mut self, s: &str) {
        self.out.extend_from_slice(s.as_bytes());
    }

    fn write_line_break(&mut self) {
        self.whitespace = true;
        self.indention = true;
        self.column = 0;
        self.out.push(b'\n');
    }

    fn write_indent(&mut self) {
        let indent = self.indent.unwrap_or(0);
        if !self.indention
            || self.column > indent
            || (self.column == indent && !self.whitespace)
        {
            self.write_line_break();
        }
        if self.column < indent {
            self.whitespace = true;
            let pad = (indent - self.column) as usize;
            self.out.extend(std::iter::repeat(b' ').take(pad));
            self.column = indent;
        }
    }

    fn write_indicator(&mut self, indicator: &str, need_whitespace: bool, whitespace: bool, indention: bool) {
        let data = if self.whitespace || !need_whitespace {
            indicator.to_string()
        } else {
            format!(" {indicator}")
        };
        self.whitespace = whitespace;
        self.indention = self.indention && indention;
        self.column += data.chars().count() as i64;
        self.open_ended = false;
        self.write(&data);
    }

    fn increase_indent(&mut self, flow: bool, indentless: bool) {
        self.indents.push(self.indent);
        match self.indent {
            None => self.indent = Some(if flow { BEST_INDENT } else { 0 }),
            Some(i) if !indentless => self.indent = Some(i + BEST_INDENT),
            _ => {}
        }
    }

    fn write_plain(&mut self, text: &str, split: bool) {
        if text.is_empty() {
            return;
        }
        if !self.whitespace {
            self.column += 1;
            self.out.push(b' ');
        }
        self.whitespace = false;
        self.indention = false;
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        let is_break = |c: char| matches!(c, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}');
        let mut spaces = false;
        let mut breaks = false;
        let mut start = 0usize;
        let mut end = 0usize;
        while end <= n {
            let ch = if end < n { Some(chars[end]) } else { None };
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end && self.column > BEST_WIDTH && split {
                        self.write_indent();
                        self.whitespace = false;
                        self.indention = false;
                    } else {
                        let data: String = chars[start..end].iter().collect();
                        self.column += (end - start) as i64;
                        self.write(&data);
                    }
                    start = end;
                }
            } else if breaks {
                if !matches!(ch, Some(c) if is_break(c)) {
                    // (no fully-plain multiline in our data; keep faithful anyway)
                    if chars[start] == '\n' {
                        self.write_line_break();
                    }
                    for &br in &chars[start..end] {
                        if br == '\n' {
                            self.write_line_break();
                        } else {
                            self.write_line_break();
                        }
                    }
                    self.write_indent();
                    self.whitespace = false;
                    self.indention = false;
                    start = end;
                }
            } else if ch.is_none() || matches!(ch, Some(c) if c == ' ' || is_break(c)) {
                let data: String = chars[start..end].iter().collect();
                self.column += (end - start) as i64;
                self.write(&data);
                start = end;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
                breaks = is_break(c);
            }
            end += 1;
        }
    }

    fn write_single_quoted(&mut self, text: &str, split: bool) {
        self.write_indicator("'", true, false, false);
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        let is_break = |c: char| matches!(c, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}');
        let mut spaces = false;
        let mut breaks = false;
        let mut start = 0usize;
        let mut end = 0usize;
        while end <= n {
            let ch = if end < n { Some(chars[end]) } else { None };
            if spaces {
                if ch.is_none() || ch != Some(' ') {
                    if start + 1 == end && self.column > BEST_WIDTH && split && start != 0 && end != n {
                        self.write_indent();
                    } else {
                        let data: String = chars[start..end].iter().collect();
                        self.column += (end - start) as i64;
                        self.write(&data);
                    }
                    start = end;
                }
            } else if breaks {
                if ch.is_none() || !matches!(ch, Some(c) if is_break(c)) {
                    if chars[start] == '\n' {
                        self.write_line_break();
                    }
                    for &br in &chars[start..end] {
                        let _ = br;
                        self.write_line_break();
                    }
                    self.write_indent();
                    start = end;
                }
            } else if ch.is_none() || matches!(ch, Some(c) if c == ' ' || is_break(c) || c == '\'') {
                if start < end {
                    let data: String = chars[start..end].iter().collect();
                    self.column += (end - start) as i64;
                    self.write(&data);
                    start = end;
                }
            }
            if ch == Some('\'') {
                self.column += 2;
                self.write("''");
                start = end + 1;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
                breaks = is_break(c);
            }
            end += 1;
        }
        self.write_indicator("'", false, false, false);
    }

    fn write_double_quoted(&mut self, text: &str, split: bool) {
        self.write_indicator("\"", true, false, false);
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        let mut start = 0usize;
        let mut end = 0usize;
        while end <= n {
            let ch = if end < n { Some(chars[end]) } else { None };
            let needs_escape = |c: char| -> bool {
                matches!(c, '"' | '\\' | '\u{85}' | '\u{2028}' | '\u{2029}' | '\u{feff}')
                    || !('\x20'..='\x7e').contains(&c) // allow_unicode = False
            };
            if ch.is_none() || matches!(ch, Some(c) if needs_escape(c)) {
                if start < end {
                    let data: String = chars[start..end].iter().collect();
                    self.column += (end - start) as i64;
                    self.write(&data);
                    start = end;
                }
                if let Some(c) = ch {
                    let data = if let Some(&(_, r)) = ESCAPE_REPLACEMENTS.iter().find(|&&(k, _)| k == c) {
                        format!("\\{r}")
                    } else if (c as u32) <= 0xff {
                        format!("\\x{:02X}", c as u32)
                    } else if (c as u32) <= 0xffff {
                        format!("\\u{:04X}", c as u32)
                    } else {
                        format!("\\U{:08X}", c as u32)
                    };
                    self.column += data.chars().count() as i64;
                    self.write(&data);
                    start = end + 1;
                }
            }
            if 0 < end
                && end < n.saturating_sub(1)
                && (ch == Some(' ') || start >= end)
                && self.column + (end as i64 - start as i64) > BEST_WIDTH
                && split
            {
                // Python slicing is lenient: text[start:end] is "" when start>=end.
                let mut data: String = if start < end {
                    chars[start..end].iter().collect()
                } else {
                    String::new()
                };
                data.push('\\');
                if start < end {
                    start = end;
                }
                self.column += data.chars().count() as i64;
                self.write(&data);
                self.write_indent();
                self.whitespace = false;
                self.indention = false;
                if chars.get(start) == Some(&' ') {
                    self.column += 1;
                    self.write("\\");
                }
            }
            end += 1;
        }
        self.write_indicator("\"", false, false, false);
    }

    /// Emit one scalar node (value already rendered to its plain text + target tag).
    fn emit_scalar(&mut self, text: &str, tag: Tag, simple_key: bool) {
        let analysis = analyze_scalar(text);
        // implicit[0]: would the plain rendering resolve back to this exact tag?
        let implicit0 = resolve_plain(text) == tag;
        // choose_scalar_style (style is always None for safe_dump).
        let style = self.choose_style(&analysis, implicit0, simple_key);
        // expect_scalar: increase_indent(flow=True) around the write.
        self.increase_indent(true, false);
        let split = !simple_key;
        match style {
            ScalarStyle::Plain => self.write_plain(text, split),
            ScalarStyle::Single => self.write_single_quoted(text, split),
            ScalarStyle::Double => self.write_double_quoted(text, split),
        }
        self.indent = self.indents.pop().unwrap();
    }

    fn choose_style(&self, a: &Analysis, implicit0: bool, simple_key: bool) -> ScalarStyle {
        // flow_level is always 0 (block); canonical=False; event.style=None.
        if implicit0
            && !(simple_key && (a.empty || a.multiline))
            && a.allow_block_plain
        {
            return ScalarStyle::Plain;
        }
        if a.allow_single_quoted && !(simple_key && a.multiline) {
            return ScalarStyle::Single;
        }
        ScalarStyle::Double
    }
}

#[derive(Clone, Copy)]
enum ScalarStyle {
    Plain,
    Single,
    Double,
}

/// Render a `serde_yaml::Value` scalar to (plain-text, target-tag) exactly as
/// PyYAML's SafeRepresenter would.
fn scalar_repr(v: &Value) -> Option<(String, Tag)> {
    match v {
        Value::Null => Some(("null".to_string(), Tag::Null)),
        Value::Bool(b) => Some(((if *b { "true" } else { "false" }).to_string(), Tag::Bool)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some((i.to_string(), Tag::Int))
            } else if let Some(u) = n.as_u64() {
                Some((u.to_string(), Tag::Int))
            } else if let Some(f) = n.as_f64() {
                Some((format_float(f), Tag::Float))
            } else {
                Some((n.to_string(), Tag::Float))
            }
        }
        Value::String(s) => Some((s.clone(), Tag::Str)),
        _ => None,
    }
}

/// PyYAML `represent_float` uses `repr(float)` with a few fixups. Corpus locks
/// carry no floats, so a best-effort Python-repr-ish rendering suffices.
fn format_float(f: f64) -> String {
    if f.is_nan() {
        return ".nan".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { ".inf".to_string() } else { "-.inf".to_string() };
    }
    let mut s = format!("{f}");
    if !s.contains('.') && !s.contains('e') && !s.contains('E') {
        s.push_str(".0");
    }
    s
}

/// Emit `value` as PyYAML `safe_dump(value, sort_keys=True,
/// default_flow_style=False)` would — byte-for-byte.
pub fn emit_yaml(value: &Value) -> Vec<u8> {
    let mut e = Emitter::new();
    emit_node(&mut e, value, false, false);
    // expect_document_end -> write_indent() -> trailing newline.
    e.write_indent();
    e.out
}

/// expect_node for block context (root/mapping-value/sequence-item).
fn emit_node(e: &mut Emitter, value: &Value, _sequence: bool, mapping: bool) {
    match value {
        Value::Mapping(m) => {
            if m.is_empty() {
                // check_empty_mapping -> flow "{}"
                e.write_indicator("{", true, true, false);
                e.write_indicator("}", false, false, false);
            } else {
                emit_block_mapping(e, m);
            }
        }
        Value::Sequence(s) => {
            if s.is_empty() {
                e.write_indicator("[", true, true, false);
                e.write_indicator("]", false, false, false);
            } else {
                emit_block_sequence(e, s, mapping);
            }
        }
        _ => {
            let (text, tag) = scalar_repr(value).expect("scalar");
            e.emit_scalar(&text, tag, false);
        }
    }
}

fn sorted_keys(m: &serde_yaml::Mapping) -> Vec<(String, &Value)> {
    let mut items: Vec<(String, &Value)> = Vec::with_capacity(m.len());
    for (k, v) in m {
        // Keys in our compose docs are always strings.
        let ks = match k {
            Value::String(s) => s.clone(),
            other => scalar_repr(other).map(|(t, _)| t).unwrap_or_default(),
        };
        items.push((ks, v));
    }
    // sort_keys=True: Python sorts (key, value) tuples; keys are unique strings.
    items.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    items
}

fn emit_block_mapping(e: &mut Emitter, m: &serde_yaml::Mapping) {
    e.increase_indent(false, false);
    let items = sorted_keys(m);
    for (k, v) in items {
        e.write_indent();
        // check_simple_key: our keys are short, non-empty, single-line -> simple.
        e.emit_scalar(&k, Tag::Str, true);
        // expect_block_mapping_simple_value: ':' with need_whitespace=False.
        e.write_indicator(":", false, false, false);
        emit_node(e, v, false, true);
    }
    e.indent = e.indents.pop().unwrap();
}

fn emit_block_sequence(e: &mut Emitter, s: &[Value], mapping_context: bool) {
    let indentless = mapping_context && !e.indention;
    e.increase_indent(false, indentless);
    for item in s {
        e.write_indent();
        e.write_indicator("-", true, false, true);
        emit_node(e, item, true, false);
    }
    e.indent = e.indents.pop().unwrap();
}

#[cfg(test)]
mod emitter_tests {
    use super::*;

    /// Every committed corpus `compose.lock.yml` IS PyYAML output. Strip the 3
    /// header comment lines, parse with serde_yaml, re-emit with our port, and
    /// require byte-identical output. This pins emitter fidelity.
    #[test]
    fn round_trips_committed_locks_byte_for_byte() {
        let stacks = [
            "insert-trim", "svcchain", "webstack", "configpipeline", "faultlab",
            "demo",
        ];
        for stack in stacks {
            let path = format!("guest/stacks/{stack}/compose.lock.yml");
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {path}: {e}"));
            // Strip leading '#' comment lines (the generated header).
            let mut body = String::new();
            let mut in_header = true;
            for line in raw.lines() {
                if in_header && line.starts_with('#') {
                    continue;
                }
                in_header = false;
                body.push_str(line);
                body.push('\n');
            }
            let value: Value = serde_yaml::from_str(&body)
                .unwrap_or_else(|e| panic!("parse {path}: {e}"));
            let emitted = emit_yaml(&value);
            let emitted_s = String::from_utf8_lossy(&emitted);
            assert_eq!(
                emitted_s, body,
                "emitter mismatch for {stack}\n--- expected ---\n{body}\n--- got ---\n{emitted_s}"
            );
        }
    }

    /// End-to-end: parse the ORIGINAL compose.yml, run the emit-lock transforms
    /// with the known pinned digests, and require the output to equal the
    /// committed compose.lock.yml byte-for-byte. Validates parse + transform +
    /// emit together (the image-based corpus stacks).
    #[test]
    fn emit_lock_matches_committed_locks() {
        const PG: &str =
            "docker.io/library/postgres@sha256:57c72fd2a128e416c7fcc499958864df5301e940bca0a56f58fddf30ffc07777";
        const PG_PIN: &str =
            "localhost/tdvmm-postgres-57c72fd2a128@sha256:cbf217007d0742829dc120c3ea9cd2621e90eb3adfeaf6684e87ce268a2ca368";
        for stack in ["faultlab", "svcchain", "configpipeline"] {
            let compose_path = format!("guest/stacks/{stack}/compose.yml");
            let raw = std::fs::read_to_string(&compose_path).unwrap();
            let doc: Value = serde_yaml::from_str(&raw).unwrap();
            let mut digests: HashMap<String, String> = HashMap::new();
            digests.insert(PG.into(), PG_PIN.into());
            // configpipeline: a build: service (worker) pinned by its output tag.
            digests.insert(
                "localhost/tdvmm-configpipeline-worker:corpus".into(),
                "localhost/tdvmm-configpipeline-worker@sha256:c05937c63df870e5f337543187d64a50975ec54794df45ed06e4182348dc4422".into(),
            );
            let project = format!("tdvmm_{}", stack.replace('-', "_"));
            let out = emit_lock(
                &doc,
                Path::new(&compose_path),
                &digests,
                "/var/lib/tdvmm-stack/binds",
                &project,
            )
            .unwrap_or_else(|e| panic!("emit_lock {stack}: {}", e.message));
            let got = String::from_utf8_lossy(&out.lock_yaml).into_owned();

            // schema-3: every service carries the event-bridge FIFO bind.
            let locked: Value = serde_yaml::from_str(&got).unwrap();
            let services = locked.get("services").and_then(|v| v.as_mapping()).unwrap();
            let want_fifo = format!("{EVENT_FIFO}:{EVENT_FIFO}:rw");
            for (name, cfg) in services {
                let present = cfg
                    .get("volumes")
                    .and_then(|v| v.as_sequence())
                    .map(|vs| vs.iter().any(|e| e.as_str() == Some(&want_fifo)))
                    .unwrap_or(false);
                assert!(present, "{stack}: service {name:?} missing the event FIFO bind");
            }

            // The committed lock is a generated file; TDVMM_REGEN_LOCKS=1 rewrites it
            // (mirrors the proto-goldens regen). A full corpus re-bake — every lock +
            // its .tdvmm artifact — is the separate follow-up.
            let lock_path = format!("guest/stacks/{stack}/compose.lock.yml");
            if std::env::var("TDVMM_REGEN_LOCKS").is_ok() {
                std::fs::write(&lock_path, got.as_bytes()).unwrap();
                continue;
            }
            let want = std::fs::read_to_string(&lock_path).unwrap();
            assert_eq!(got, want, "emit_lock mismatch for {stack}");
        }
    }
}
