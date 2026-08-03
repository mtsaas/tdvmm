#!/usr/bin/env bash
# deterministic-vmm OP-1a acceptance gate: the .dvmm single-file artifact.
#
# Proves every OP-1a property WITHOUT re-baking (it packs the already-built boot
# artifacts of each stack). For each stack it:
#
#   1. BIT-REPRODUCIBLE: pack the same inputs twice -> byte-identical .dvmm.
#   2. INSPECT: prints valid manifest JSON, fast (manifest member only).
#   3. VERIFY: passes on the good artifact (exit 0).
#   4. RUN-FROM-ARTIFACT (FF): `dvmm run <stack>.dvmm` boots offline under FF,
#      rows ascend + cap, per-hop mean <= 500us (the VMM property).
#   5. OFFLINE: the same run under `unshare -rn` (networking blocked) still works.
#
# Plus, once (on the first stack), the cross-cutting gates:
#   6. VERIFY catches a flipped byte (nonzero) and `run` refuses to boot it.
#   7. OVERRIDE PRECEDENCE: baked run-defaults < CLI flags; the effective-config
#      provenance line reflects it.
#   8. RUN==BOOT: `dvmm run <stack>.dvmm` matches the raw `dvmm boot` path
#      (same kernel/initramfs/cmdline) in cadence + per-hop gate.
#
# Exits 0 only if every gate passes.
#
# Usage: scripts/artifact_test.sh [stack ...]     (default: insert-trim svcchain)
# Env:   MEM(3072)  INTERVAL(3)  MAX_ROWS(5)  HORIZON(24s)  WALL_TIMEOUT(120)
#        GATE_HOP_US(500)
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/target/release/dvmm"
KERNEL="$ROOT/guest/kernel/vmlinux-6.1.128"
# Self-contained bake outputs: a gitignored, persistent test cache dir (NOT the
# repo, NOT ~/.dvmm). The per-stack initramfs (needed by gate 8's raw boot) lands
# in $CACHE/artifacts/; keeping the dir warm across runs makes re-bakes fast.
CACHE="${DVMM_TEST_CACHE:-$ROOT/.dvmm-tmp/dvmm-cache}"; mkdir -p "$CACHE"

STACKS=("$@"); [ "${#STACKS[@]}" -eq 0 ] && STACKS=(insert-trim svcchain)
MEM="${MEM:-3072}"
INTERVAL="${INTERVAL:-3}"
MAX_ROWS="${MAX_ROWS:-5}"
HORIZON="${HORIZON:-24s}"
WALL_TIMEOUT="${WALL_TIMEOUT:-120}"
GATE_HOP_US="${GATE_HOP_US:-500}"

[ -x "$BIN" ] || { echo "[artifact] building dvmm..."; ( cd "$ROOT" && cargo build --release ) || exit 3; }
[ -f "$KERNEL" ] || { echo "[artifact] kernel missing: $KERNEL"; exit 3; }

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
CMDLINE="console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable dvmm.stack=1 dvmm.interval=$INTERVAL dvmm.maxrows=$MAX_ROWS dvmm.hc_tick=2"

# ascending unique prefix of the row sequence, e.g. "1 2 3 4 5".
asc_prefix() { grep -oE 'DVMM_ROWCOUNT=[0-9]+' "$1" | cut -d= -f2 | awk '!seen[$0]++'; }
rows_ok() {  # <log> : started low, non-decreasing, capped at MAX_ROWS, never over
  grep -oE 'DVMM_ROWCOUNT=[0-9]+' "$1" | cut -d= -f2 | awk -v cap="$MAX_ROWS" '
    { v=$1+0; n++;
      if (n==1 && v<=2) low=1; if (n>1 && v<prev) nondec=0; if (v>mx) mx=v;
      if (v>cap) over=1; prev=v }
    BEGIN{nondec=1}
    END{ exit !(n>=1 && low==1 && nondec==1 && over!=1 && mx==cap) }'
}
hop_mean_us() { awk '$1=="hop_ns_mean"{printf "%.3f", $2/1000}' "$1"; }

overall=0
first=1
for stack in "${STACKS[@]}"; do
  echo "==================================================================="
  echo " ARTIFACT GATES: $stack"
  echo "==================================================================="
  compose="$ROOT/guest/stacks/$stack/compose.yml"
  if [ ! -f "$compose" ]; then
    echo "  SKIP: $stack has no compose.yml at $compose"; continue
  fi
  # `dvmm build --cache-dir "$CACHE"` writes the per-stack initramfs here (gate 8's raw boot).
  initrd="$CACHE/artifacts/initramfs-alpine-${stack}.cpio.gz"
  A="$TMP/$stack-A.dvmm"; B="$TMP/$stack-B.dvmm"
  ok=1

  # (1) bit-reproducible: `dvmm build` the SAME compose twice -> byte-identical .dvmm
  #     (OP-1b folds the whole bake into the binary; the initramfs IS now bit-repro).
  #     Bake to the canonical <stack>.dvmm basename (so the committed stack.lock's
  #     recorded artifact filename is unchanged), then copy each result aside to compare.
  "$BIN" build "$compose" -o "$TMP/$stack.dvmm" --cache-dir "$CACHE" >/dev/null 2>"$TMP/packA.err" || { echo "  FAIL: build A"; tail -5 "$TMP/packA.err"; overall=1; continue; }
  cp "$TMP/$stack.dvmm" "$A"
  "$BIN" build "$compose" -o "$TMP/$stack.dvmm" --cache-dir "$CACHE" >/dev/null 2>"$TMP/packB.err" || { echo "  FAIL: build B"; tail -5 "$TMP/packB.err"; overall=1; continue; }
  cp "$TMP/$stack.dvmm" "$B"
  shA="$(sha256sum "$A" | awk '{print $1}')"; shB="$(sha256sum "$B" | awk '{print $1}')"
  if [ "$shA" = "$shB" ]; then echo "  (1) BIT-REPRODUCIBLE OK: two builds -> identical .dvmm ($shA)"; else echo "  (1) FAIL: .dvmm differs ($shA != $shB)"; ok=0; fi

  # (2) inspect fast + valid JSON
  t0=$(date +%s.%N)
  "$BIN" inspect "$A" > "$TMP/$stack.manifest.json" 2>/dev/null
  t1=$(date +%s.%N)
  if python3 -c "import json,sys; json.load(open('$TMP/$stack.manifest.json'))" 2>/dev/null; then
    echo "  (2) INSPECT OK: valid manifest JSON in $(awk "BEGIN{printf \"%.3f\", $t1-$t0}")s (manifest member only)"
  else echo "  (2) FAIL: inspect did not emit valid JSON"; ok=0; fi

  # (3) verify good
  if "$BIN" verify "$A" >/dev/null 2>&1; then echo "  (3) VERIFY OK: member hashes match manifest"; else echo "  (3) FAIL: verify on good artifact"; ok=0; fi

  # (4) run-from-artifact under FF
  runlog="$TMP/$stack.run.log"; metrics="$TMP/$stack.metrics"
  timeout "$WALL_TIMEOUT" "$BIN" run "$A" --cmdline "$CMDLINE" --max-virtual-time "$HORIZON" \
    --metrics-out "$metrics" </dev/null >"$runlog" 2>&1
  rc=$?
  hm="$(hop_mean_us "$metrics" 2>/dev/null)"; [ -z "$hm" ] && hm=0
  if [ "$rc" = "3" ] && rows_ok "$runlog" && awk "BEGIN{exit !($hm>0 && $hm<=$GATE_HOP_US)}"; then
    echo "  (4) RUN-FROM-ARTIFACT OK: rc=$rc rows=[$(asc_prefix "$runlog" | tr '\n' ' ')] per-hop mean ${hm}us <= ${GATE_HOP_US}us"
  else
    echo "  (4) FAIL: run rc=$rc rows=[$(asc_prefix "$runlog" | tr '\n' ' ')] hop_mean=${hm}us"; tail -8 "$runlog" | sed 's/^/      /'; ok=0
  fi

  # (5) offline (networking blocked)
  offlog="$TMP/$stack.offline.log"
  unshare -rn timeout "$WALL_TIMEOUT" "$BIN" run "$A" --cmdline "$CMDLINE" --max-virtual-time "$HORIZON" \
    </dev/null >"$offlog" 2>&1
  orc=$?
  if [ "$orc" = "3" ] && rows_ok "$offlog"; then
    echo "  (5) OFFLINE OK: rc=$orc under unshare -rn (no network); rows=[$(asc_prefix "$offlog" | tr '\n' ' ')]"
  else echo "  (5) FAIL: offline run rc=$orc"; tail -8 "$offlog" | sed 's/^/      /'; ok=0; fi

  # ---- cross-cutting gates, once (on the first available stack) -------------
  if [ "$first" = 1 ]; then
    first=0

    # (6) verify catches corruption + run refuses
    C="$TMP/$stack-corrupt.dvmm"; cp "$A" "$C"
    python3 -c "
import os
p='$C'; sz=os.path.getsize(p); off=sz//2
with open(p,'r+b') as f: f.seek(off); b=f.read(1); f.seek(off); f.write(bytes([b[0]^0xff]))
"
    if "$BIN" verify "$C" >/dev/null 2>&1; then echo "  (6) FAIL: verify passed on a corrupted artifact"; ok=0
    else
      if "$BIN" run "$C" --max-virtual-time 3s </dev/null >"$TMP/corrupt.run.log" 2>&1; then
        echo "  (6) FAIL: run booted a corrupted artifact"; ok=0
      elif grep -q 'MISMATCH' "$TMP/corrupt.run.log"; then
        echo "  (6) CORRUPTION OK: verify FAILS (nonzero) + run REFUSES (hash mismatch) on a flipped byte"
      else echo "  (6) FAIL: run did not report a hash mismatch"; ok=0; fi
    fi

    # (7) override precedence: baked ff/mem/horizon overridden by flags
    ovlog="$TMP/override.log"
    timeout "$WALL_TIMEOUT" "$BIN" run "$A" --mem 2048 --ff off --max-virtual-time 8s \
      --cmdline "$CMDLINE" </dev/null >"$ovlog" 2>&1
    eff="$(grep 'effective-config:' "$ovlog" | head -1)"
    echo "      $eff"
    if echo "$eff" | grep -q 'mem=2048 (flag)' && echo "$eff" | grep -q 'ff=off (flag)' \
       && echo "$eff" | grep -q 'max-virtual-time=8s (flag)' && echo "$eff" | grep -q 'cmdline=.* (flag)'; then
      echo "  (7) OVERRIDE PRECEDENCE OK: baked < flag reflected in the provenance line"
    else echo "  (7) FAIL: override provenance not as expected"; ok=0; fi

    # (8) run == boot (same kernel/initramfs/cmdline via the two verbs)
    blog="$TMP/boot.log"
    timeout "$WALL_TIMEOUT" "$BIN" boot --kernel "$KERNEL" --initrd "$initrd" --mem "$MEM" --ff on \
      --cmdline "$CMDLINE" --max-virtual-time "$HORIZON" --metrics-out "$TMP/boot.metrics" \
      </dev/null >"$blog" 2>&1
    rprefix="$(asc_prefix "$runlog" | tr '\n' ' ')"; bprefix="$(asc_prefix "$blog" | tr '\n' ' ')"
    bhm="$(hop_mean_us "$TMP/boot.metrics" 2>/dev/null)"; [ -z "$bhm" ] && bhm=0
    if [ "$rprefix" = "$bprefix" ] && rows_ok "$blog" && awk "BEGIN{exit !($bhm>0 && $bhm<=$GATE_HOP_US)}"; then
      echo "  (8) RUN==BOOT OK: identical row cadence [$rprefix]; run hop ${hm}us / boot hop ${bhm}us (both <= ${GATE_HOP_US}us)"
    else echo "  (8) FAIL: run/boot mismatch (run=[$rprefix] boot=[$bprefix] run_hop=${hm} boot_hop=${bhm})"; ok=0; fi
  fi

  if [ "$ok" = 1 ]; then echo "  => $stack: ALL ARTIFACT GATES PASS"; else echo "  => $stack: FAIL"; overall=1; fi
  echo
done

if [ "$overall" = 0 ]; then echo "ARTIFACT TEST PASS: every gate held."; exit 0; fi
echo "ARTIFACT TEST FAIL: see above."; exit 1
