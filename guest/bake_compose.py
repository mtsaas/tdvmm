#!/usr/bin/env python3
"""deterministic-vmm Phase-2a compose processor.

The static, loud boundary of the 2a pipeline. Two jobs:

  validate   Parse a compose.yml, enforce the SUPPORTED SUBSET, and reject
             everything outside it with a clear, greppable diagnostic
             (DVMM_BAKE_REJECT: ...). Also emits, as JSON on stdout, the facts
             bake-stack needs next: the set of images to bake, the relative
             read-only binds to materialize, and any warnings (e.g. stripped
             ports). Runs BEFORE any pull/bake, so an unsupported stack fails
             fast and cheap.

  emit-lock  Given the resolved image digests + the in-guest bind base path +
             the pinned project name, write the deterministic compose.lock.yml
             (the ONLY compose file the guest ever sees) and a bind copy-manifest.
             Re-runs validation first, so the lock can never be emitted for a
             stack that would be rejected.

Supported subset (2a + 2b): image-based services, HOST-SIDE build: contexts
(built at bake time, base images digest-pinned), relative READ-ONLY *and*
READ-WRITE binds (materialized into the guest image; rw writes are ephemeral),
named volumes (tmpfs-backed, ephemeral), container healthchecks +
depends_on: {condition: service_healthy} gates (resolved at runtime by the
guest-side healthcheck ticker -- 2b items 3&4), service-started depends_on, and
private compose networks. Everything else is rejected loudly (absolute host
binds, external networks, pull_policy: always, network_mode: host, unpinned
build bases). A multi-stack "real-world" corpus remains a later 2b item.

build: (2b) is host-side ONLY — a service's Containerfile/Dockerfile is built on
the host at bake time (never inside the guest), its FROM bases must be
digest-pinned, and the built image flows through the SAME squash / seed-store /
compose.lock pipeline as any pulled image.
"""
import argparse
import json
import os
import sys

try:
    import yaml
except ImportError:
    sys.stderr.write("DVMM_BAKE_ERROR: python3 pyyaml is required on the build host\n")
    sys.exit(2)

REJECT = "DVMM_BAKE_REJECT"
WARN = "DVMM_BAKE_WARN"


def die_reject(msg):
    sys.stderr.write(f"{REJECT}: {msg}\n")
    sys.exit(3)


def die_error(msg):
    sys.stderr.write(f"DVMM_BAKE_ERROR: {msg}\n")
    sys.exit(2)


def load(path):
    try:
        with open(path) as f:
            doc = yaml.safe_load(f)
    except FileNotFoundError:
        die_error(f"compose file not found: {path}")
    except yaml.YAMLError as e:
        die_error(f"could not parse {path}: {e}")
    if not isinstance(doc, dict):
        die_reject(f"{path}: top level is not a mapping")
    return doc


def split_bind(entry, service):
    """Return (src, target, mode) for a service volume entry, or None if it is a
    NAMED volume mount (kept as-is). Long-form dict binds are also handled."""
    if isinstance(entry, str):
        parts = entry.split(":")
        if len(parts) == 1:
            # anonymous volume (a bare container path) -> kept, not a bind
            return None
        src, target = parts[0], parts[1]
        mode = parts[2] if len(parts) > 2 else "rw"
        # A named volume: source has no path separator and is not ./ ../ / ~
        looks_like_path = src.startswith((".", "/", "~")) or "/" in src
        if not looks_like_path:
            return None  # named volume, kept
        return (src, target, mode)
    if isinstance(entry, dict):
        vtype = entry.get("type", "volume")
        if vtype == "bind":
            src = entry.get("source", "")
            target = entry.get("target", "")
            mode = "ro" if entry.get("read_only") else "rw"
            return (src, target, mode)
        return None  # volume/tmpfs long form -> kept
    return None


def build_output_tag(sname, scfg):
    """The image tag a build: service resolves to: an explicit image: if given
    (compose names the built result), else a deterministic synthesized tag. Must
    agree between validate and emit-lock, so both call this."""
    img = scfg.get("image")
    if img:
        return img
    safe = "".join(c if (c.isalnum() or c in "-_.") else "-" for c in sname)
    return f"localhost/dvmm-build-{safe}:baked"


def parse_dockerfile_froms(dockerfile_path):
    """Return the list of external base references in a Dockerfile's FROM lines
    (i.e. excluding `FROM <prior-stage-alias>`). Stage aliases (`AS name`) are
    tracked so a later `FROM name` is not mistaken for an external image."""
    stages = set()
    bases = []
    with open(dockerfile_path) as f:
        for raw in f:
            line = raw.strip()
            if not line or line.startswith("#"):
                continue
            toks = line.split()
            if len(toks) >= 2 and toks[0].upper() == "FROM":
                ref = toks[1]
                # `FROM <ref> AS <alias>` -> record the alias.
                if len(toks) >= 4 and toks[2].upper() == "AS":
                    stages.add(toks[3])
                if ref not in stages:  # external image, not a prior stage
                    bases.append(ref)
    return bases


def validate_build(build, sname, path, scfg):
    """Validate a service's build: context (host-side build at bake time). Returns
    a dict {service, context(abs), dockerfile(abs), image_tag, bases[]}."""
    # Accept the short string form (context dir) and the long dict form.
    if isinstance(build, str):
        context, dockerfile = build, "Dockerfile"
    elif isinstance(build, dict):
        unsupported = set(build) - {"context", "dockerfile"}
        if unsupported:
            die_reject(
                f"service '{sname}' build: uses unsupported key(s) {sorted(unsupported)}. "
                f"Only 'context' and 'dockerfile' are supported (build args/target "
                f"would compromise closed-world reproducibility). Remove them."
            )
        context = build.get("context", ".")
        dockerfile = build.get("dockerfile", "Dockerfile")
    else:
        die_reject(f"service '{sname}' build: must be a path or a mapping.")

    if context.startswith(("/", "~")):
        die_reject(
            f"service '{sname}' build context '{context}' is absolute. Only a "
            f"RELATIVE build context (next to the compose file) is supported, so "
            f"the bake is self-contained and closed-world."
        )
    base = os.path.dirname(path)
    abs_ctx = os.path.normpath(os.path.join(base, context))
    if not os.path.isdir(abs_ctx):
        die_reject(f"service '{sname}' build context '{context}' is not a directory ({abs_ctx}).")
    abs_df = os.path.normpath(os.path.join(abs_ctx, dockerfile))
    if not os.path.isfile(abs_df):
        die_reject(
            f"service '{sname}' build dockerfile '{dockerfile}' not found in the "
            f"context ({abs_df})."
        )

    bases = parse_dockerfile_froms(abs_df)
    if not bases:
        die_reject(f"service '{sname}' build {dockerfile} has no FROM instruction.")
    for b in bases:
        if "@sha256:" not in b:
            die_reject(
                f"service '{sname}' build base image '{b}' is NOT digest-pinned. "
                f"Every external FROM must be pinned by @sha256:<digest> so the "
                f"host-side build is reproducible + closed-world. Pin it."
            )

    return {
        "service": sname,
        "context": abs_ctx,
        "dockerfile": abs_df,
        "image_tag": build_output_tag(sname, scfg),
        "bases": bases,
    }


def validate(doc, path):
    """Enforce the supported subset. Returns (images, builds, binds, warnings)."""
    warnings = []
    services = doc.get("services")
    if not isinstance(services, dict) or not services:
        die_reject(f"{path}: no services defined")

    # ---- networks: reject external ----
    for nname, ncfg in (doc.get("networks") or {}).items():
        if isinstance(ncfg, dict) and ncfg.get("external"):
            die_reject(
                f"network '{nname}' is declared external:. The closed-world guest "
                f"cannot join a pre-existing host network. Remove 'external: true' "
                f"and let compose create a private network."
            )

    images = []
    builds = []
    binds = []
    for sname, scfg in services.items():
        if not isinstance(scfg, dict):
            die_reject(f"service '{sname}': not a mapping")

        # ---- image: (pull) XOR build: (host-side build at bake time) ----
        image = scfg.get("image")
        if "build" in scfg:
            # A build: service is built host-side; image: (if present) NAMES the
            # built result, it is NOT pulled. So it does not go in `images`.
            builds.append(validate_build(scfg["build"], sname, path, scfg))
        else:
            if not image:
                die_reject(
                    f"service '{sname}' has no image: and no build:. A service must "
                    f"reference a pulled image: or provide a build: context."
                )
            if image not in images:
                images.append(image)

        # ---- pull_policy: always ----
        pp = scfg.get("pull_policy")
        if pp == "always":
            die_reject(
                f"service '{sname}' sets pull_policy: always. The guest runs "
                f"--pull=never in a closed world; an always-pull can never be "
                f"satisfied offline. Remove pull_policy or set it to 'never'."
            )

        # ---- healthchecks (2b items 3&4): SUPPORTED via the guest-side ticker.
        # No bake-time rejection: podman has no systemd auto-runner, so the guest
        # runs `podman healthcheck run` on the interval (see the healthcheck
        # ticker in the guest launch path), which resolves service_healthy gates.
        # The healthcheck block is passed through into compose.lock.yml as-is.

        # ---- network_mode: host breaks the closed world ----
        if scfg.get("network_mode") == "host":
            die_reject(
                f"service '{sname}' uses network_mode: host. The closed world "
                f"forbids host networking. Use a private compose network."
            )

        # ---- depends_on: service_healthy gates are SUPPORTED (2b items 3&4).
        # compose blocks `up` until the dependency reports healthy; the guest-side
        # healthcheck ticker makes that happen. No bake-time rejection. (A
        # dependency that declares such a gate but has NO healthcheck would hang;
        # that is the stack author's error, surfaced at runtime, not a bake reject.)

        # ---- ports: warn + strip ----
        if scfg.get("ports"):
            warnings.append(
                f"service '{sname}': published ports {scfg['ports']} STRIPPED "
                f"(closed world has no host to publish to)."
            )

        # ---- volumes: classify binds ----
        for entry in scfg.get("volumes") or []:
            b = split_bind(entry, sname)
            if b is None:
                continue  # named/anonymous volume -> kept
            src, target, mode = b
            if src.startswith(("/", "~")):
                die_reject(
                    f"service '{sname}' binds absolute host path '{src}'. The "
                    f"closed-world guest has no host filesystem. Only RELATIVE "
                    f"binds (materialized into the guest image) are supported."
                )
            # relative bind (ro OR rw) -> materialize into the guest image.
            # 2b: rw is supported too; writes go to the guest tmpfs and are
            # EPHEMERAL (lost on reboot -- fine in the closed, single-writer world).
            # Absolute host binds stay rejected above (closed-world boundary).
            norm_mode = "ro" if "ro" in mode.split(",") else "rw"
            base = os.path.dirname(path)
            abssrc = os.path.normpath(os.path.join(base, src))
            if not os.path.exists(abssrc):
                die_reject(
                    f"service '{sname}' bind source '{src}' does not exist next to "
                    f"the compose file ({abssrc})."
                )
            binds.append({
                "service": sname,
                "src": abssrc,
                "rel": src,
                "target": target,
                "mode": norm_mode,
                "basename": os.path.basename(src.rstrip("/")),
            })

    return images, builds, binds, warnings


def cmd_validate(args):
    doc = load(args.compose)
    images, builds, binds, warnings = validate(doc, args.compose)
    for w in warnings:
        sys.stderr.write(f"{WARN}: {w}\n")
    json.dump(
        {"images": images, "builds": builds, "binds": binds, "warnings": warnings},
        sys.stdout,
    )
    sys.stdout.write("\n")


def cmd_emit_lock(args):
    doc = load(args.compose)
    images, builds, binds, warnings = validate(doc, args.compose)
    for w in warnings:
        sys.stderr.write(f"{WARN}: {w}\n")

    digests = json.loads(args.digests)  # {original-image-ref: pinned repo@sha256}

    # bind copy-manifest + in-guest dest path per bind (namespaced by service).
    manifest_lines = []
    dest_of = {}
    for b in binds:
        dest = f"{args.binds_base}/{b['service']}/{b['basename']}"
        dest_of[(b["service"], b["target"])] = dest
        manifest_lines.append(f"{b['src']}\t{b['service']}/{b['basename']}")

    # ---- transform the doc into the lockfile ----
    doc["name"] = args.project  # pin COMPOSE_PROJECT_NAME deterministically
    for sname, scfg in doc["services"].items():
        if "build" in scfg:
            # Host-side build: the guest never builds. Drop build: and pin image:
            # to the digest of the built+seeded image (keyed by its output tag).
            tag = build_output_tag(sname, scfg)
            scfg.pop("build", None)
            if tag in digests:
                scfg["image"] = digests[tag]
            else:
                die_error(
                    f"service '{sname}' build output '{tag}' was not baked/pinned "
                    f"(no digest supplied to emit-lock)."
                )
        else:
            img = scfg.get("image")
            if img in digests:
                scfg["image"] = digests[img]
        scfg.pop("ports", None)          # stripped (warned above)
        scfg.pop("pull_policy", None)    # guest is always --pull=never
        # rewrite relative binds (ro OR rw) to in-guest absolute paths, preserving
        # the mode. Named/anonymous volumes (split_bind -> None) are kept as-is.
        newvols = []
        for entry in scfg.get("volumes") or []:
            b = split_bind(entry, sname)
            if b is not None and not b[0].startswith(("/", "~")):
                src, target, mode = b
                dest = dest_of.get((sname, target))
                if dest:
                    norm_mode = "ro" if "ro" in mode.split(",") else "rw"
                    newvols.append(f"{dest}:{target}:{norm_mode}")
                    continue
            newvols.append(entry)
        if newvols:
            scfg["volumes"] = newvols

    with open(args.out, "w") as f:
        f.write("# GENERATED by bake_compose.py -- do NOT edit. The only compose\n")
        f.write("# file the guest ever sees. Images pinned by digest; ports stripped;\n")
        f.write("# relative RO binds materialized to in-guest paths; project pinned.\n")
        yaml.safe_dump(doc, f, sort_keys=True, default_flow_style=False)

    with open(args.binds_manifest, "w") as f:
        for line in manifest_lines:
            f.write(line + "\n")


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    v = sub.add_parser("validate")
    v.add_argument("compose")
    v.set_defaults(func=cmd_validate)
    e = sub.add_parser("emit-lock")
    e.add_argument("compose")
    e.add_argument("--digests", required=True)
    e.add_argument("--binds-base", required=True)
    e.add_argument("--project", required=True)
    e.add_argument("--out", required=True)
    e.add_argument("--binds-manifest", required=True)
    e.set_defaults(func=cmd_emit_lock)
    args = ap.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
