#!/usr/bin/env bash
# dvmm — the single tiered e2e test runner (goal-3a).
#
#   scripts/test.sh --fast      # T0 + T1        (the pre-push / inner loop)
#   scripts/test.sh --merge     # T0 + T1 + T2   (the PR gate)
#   scripts/test.sh --nightly   # everything     (T0..T3)
#   scripts/test.sh <name> ...  # run named test(s) only
#   scripts/test.sh --list      # print the coverage inventory and exit
#
# Everything is driven by tests/manifest.toml (tier, tags, timeout, requires,
# cmd). Scheduling:
#   * tests tagged `parallel-safe` run in a bounded PARALLEL POOL (JOBS wide);
#   * everything else (perf-exclusive / needs-bake) runs SERIALLY in a quiet
#     section AFTER the pool drains -- so per-hop/wall assertions never race a
#     loaded host, and two bakes never clobber the same committed artifact.
#
# Output: a per-test PASS/FAIL/SKIP summary table + wall timings, a JSON results
# file in the shared TEST-1a schema:1 shape, and the shared exit-code contract:
#   0 = every selected test passed
#   1 = at least one test FAILED (or timed out)
#   2 = an infrastructure error (bad manifest, unknown test, build failure, or a
#       test could not run -- exit 3 by the scripts' missing-prereq convention)
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
MANIFEST="$ROOT/tests/manifest.toml"
BIN="$ROOT/target/release/dvmm"

JOBS="${JOBS:-4}"                         # parallel pool width (RAM-bound ~3GiB/guest)
# Keep bake-heavy scratch OFF the small /tmp tmpfs (per env guidance).
export TMPDIR="${TMPDIR:-$ROOT/.dvmm-tmp}"
mkdir -p "$TMPDIR"
RESULTS="${DVMM_TEST_RESULTS:-$ROOT/.dvmm-test-results}"

# US (ASCII 31) is the field delimiter everywhere: it is NON-whitespace, so `read`
# preserves empty fields (a whitespace IFS like TAB collapses `a\t\tb`).
US=$'\x1f'

# ---------------------------------------------------------------------------
# manifest parser: emit one US-separated record per test:
#   name US tier US tags(space) US timeout US requires(space) US cmd US desc
# ---------------------------------------------------------------------------
parse_manifest() {
  awk '
    BEGIN{ S=sprintf("%c",31) }
    function trim(s){ sub(/^[ \t]+/,"",s); sub(/[ \t]+$/,"",s); return s }
    function unq(s){ sub(/^"/,"",s); sub(/"$/,"",s); return s }
    function arr(s){ sub(/^\[/,"",s); sub(/\]$/,"",s); gsub(/"/,"",s); gsub(/,/," ",s); return trim(s) }
    function emit(){ if(name!="") printf "%s%s%s%s%s%s%s%s%s%s%s%s%s\n",
                       name,S,tier,S,tags,S,timeout,S,requires,S,cmd,S,desc }
    /^\[\[test\]\]/ { emit(); name="";tier="";tags="";timeout="";requires="";cmd="";desc=""; next }
    /^[a-z_]+[ ]*=/ {
      key=$1; eq=index($0,"="); val=trim(substr($0,eq+1));
      if(key=="name") name=unq(val);
      else if(key=="tier") tier=unq(val);
      else if(key=="tags") tags=arr(val);
      else if(key=="timeout") timeout=val;
      else if(key=="requires") requires=unq(val);
      else if(key=="cmd") cmd=unq(val);
      else if(key=="desc") desc=unq(val);
    }
    END{ emit() }
  ' "$MANIFEST"
}

has_tag() { case " $1 " in *" $2 "*) return 0;; *) return 1;; esac }

# ---------------------------------------------------------------------------
# argument parsing
# ---------------------------------------------------------------------------
[ -f "$MANIFEST" ] || { echo "test.sh: manifest not found: $MANIFEST" >&2; exit 2; }

MODE=""; declare -a WANT_NAMES=()
for a in "$@"; do
  case "$a" in
    --fast)    MODE="fast" ;;
    --merge)   MODE="merge" ;;
    --nightly) MODE="nightly" ;;
    --list)    MODE="list" ;;
    --jobs=*)  JOBS="${a#--jobs=}" ;;
    -h|--help) grep -E '^#( |$)' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    --*)       echo "test.sh: unknown flag: $a" >&2; exit 2 ;;
    *)         WANT_NAMES+=("$a") ;;
  esac
done
if [ -z "$MODE" ] && [ "${#WANT_NAMES[@]}" -eq 0 ]; then
  echo "usage: test.sh [--fast|--merge|--nightly|--list] | test.sh <name> ..." >&2
  exit 2
fi

tier_in_mode() {
  case "$MODE" in
    fast)    case "$1" in T0|T1) return 0;; esac; return 1 ;;
    merge)   case "$1" in T0|T1|T2) return 0;; esac; return 1 ;;
    nightly) return 0 ;;
    *)       return 1 ;;
  esac
}

# ---------------------------------------------------------------------------
# --list: coverage inventory
# ---------------------------------------------------------------------------
if [ "$MODE" = "list" ]; then
  printf '%-22s %-4s %-4s %-4s %-24s %s\n' "NAME" "TIER" "FAST" "MRG" "TAGS" "DESC"
  printf '%s\n' "-------------------------------------------------------------------------------------------------------------"
  while IFS="$US" read -r name tier tags timeout requires cmd desc; do
    [ -z "$name" ] && continue
    f="-"; m="-"
    case "$tier" in T0|T1) f="x"; m="x";; T2) m="x";; esac
    printf '%-22s %-4s %-4s %-4s %-24s %s\n' "$name" "$tier" "$f" "$m" "$tags" "$desc"
  done < <(parse_manifest)
  echo
  echo "FAST = T0+T1 ; MRG(merge) = T0+T1+T2 ; nightly = all. (fast ⊂ merge ⊂ nightly)"
  exit 0
fi

# ---------------------------------------------------------------------------
# select tests
# ---------------------------------------------------------------------------
declare -a POOL=() SERIAL=() SELECTED=()
name_wanted() { for w in "${WANT_NAMES[@]}"; do [ "$w" = "$1" ] && return 0; done; return 1; }

while IFS="$US" read -r name tier tags timeout requires cmd desc; do
  [ -z "$name" ] && continue
  if [ "${#WANT_NAMES[@]}" -gt 0 ]; then
    name_wanted "$name" || continue
  else
    tier_in_mode "$tier" || continue
  fi
  rec="${name}${US}${tier}${US}${tags}${US}${timeout}${US}${requires}${US}${cmd}${US}${desc}"
  SELECTED+=("$rec")
  if has_tag "$tags" "parallel-safe"; then POOL+=("$rec"); else SERIAL+=("$rec"); fi
done < <(parse_manifest)

if [ "${#WANT_NAMES[@]}" -gt 0 ] && [ "${#SELECTED[@]}" -ne "${#WANT_NAMES[@]}" ]; then
  echo "test.sh: one or more requested tests are not in the manifest (${WANT_NAMES[*]})" >&2
  echo "  run: scripts/test.sh --list" >&2
  exit 2
fi
[ "${#SELECTED[@]}" -gt 0 ] || { echo "test.sh: no tests selected" >&2; exit 2; }

SUITE="${MODE:-named}"
rm -rf "$RESULTS"; mkdir -p "$RESULTS"

echo "==================================================================="
echo " dvmm test.sh  suite=$SUITE  jobs=$JOBS  selected=${#SELECTED[@]}"
echo "   pool (parallel-safe): ${#POOL[@]}    serial (perf/bake): ${#SERIAL[@]}"
echo "   results: $RESULTS   TMPDIR=$TMPDIR"
echo "==================================================================="

# ---------------------------------------------------------------------------
# build the release binary once, upfront (so pool tests never race on it)
# ---------------------------------------------------------------------------
echo "[build] cargo build --release ..."
if ! ( cd "$ROOT" && cargo build --release ) >"$RESULTS/_build.log" 2>&1; then
  echo "test.sh: cargo build --release FAILED (infra):" >&2
  tail -20 "$RESULTS/_build.log" >&2
  exit 2
fi
[ -x "$BIN" ] || { echo "test.sh: binary missing after build: $BIN" >&2; exit 2; }

# ---------------------------------------------------------------------------
# run one test -> writes $RESULTS/<name>.result (US-sep):
#   name US tier US tags US outcome US rc US start US end US wall US timed_out
# ---------------------------------------------------------------------------
run_one() {
  local name="$1" tier="$2" tags="$3" timeout="$4" requires="$5" cmd="$6"
  local log="$RESULTS/$name.log" start end rc outcome timed_out=0 missing=""
  for r in $requires; do [ -e "$ROOT/$r" ] || missing="$missing $r"; done
  start=$(date +%s.%N)
  if [ -n "$missing" ]; then
    outcome="skip"; rc=0
    { echo "SKIP: missing required artifact(s):$missing"; echo "cmd would have been: $cmd"; } > "$log"
  else
    ( cd "$ROOT" && exec timeout --preserve-status "$timeout" bash -c "$cmd" ) >"$log" 2>&1
    rc=$?
    case "$rc" in
      0)   outcome="pass" ;;
      124) outcome="fail"; timed_out=1 ;;   # killed by timeout
      3)   outcome="error" ;;               # scripts' missing-prereq/build convention
      *)   outcome="fail" ;;
    esac
  fi
  end=$(date +%s.%N)
  local wall; wall=$(awk "BEGIN{printf \"%.1f\", $end-$start}")
  printf '%s%s%s%s%s%s%s%s%s%s%s%s%s%s%s%s%s\n' \
    "$name" "$US" "$tier" "$US" "$tags" "$US" "$outcome" "$US" "$rc" "$US" \
    "$start" "$US" "$end" "$US" "$wall" "$US" "$timed_out" \
    > "$RESULTS/$name.result"
  local ph="serial"; has_tag "$tags" parallel-safe && ph="pool"
  printf '  [%-6s] %-22s %-6s %ss\n' "$ph" "$name" "$(echo "$outcome" | tr a-z A-Z)" "$wall"
}

SUITE_START=$(date +%s.%N)

# ---- Phase A: the parallel pool ----
POOL_START=$(date +%s.%N)
if [ "${#POOL[@]}" -gt 0 ]; then
  echo
  echo "---- Phase A: parallel pool (${#POOL[@]} tests, up to $JOBS concurrent) ----"
  running=0
  for rec in "${POOL[@]}"; do
    IFS="$US" read -r name tier tags timeout requires cmd desc <<< "$rec"
    run_one "$name" "$tier" "$tags" "$timeout" "$requires" "$cmd" &
    running=$((running+1))
    if [ "$running" -ge "$JOBS" ]; then wait -n 2>/dev/null || true; running=$((running-1)); fi
  done
  wait
fi
POOL_END=$(date +%s.%N)

# ---- Phase B: the serial quiet section ----
if [ "${#SERIAL[@]}" -gt 0 ]; then
  echo
  echo "---- Phase B: serial quiet section (${#SERIAL[@]} tests: perf-exclusive / needs-bake) ----"
  for rec in "${SERIAL[@]}"; do
    IFS="$US" read -r name tier tags timeout requires cmd desc <<< "$rec"
    run_one "$name" "$tier" "$tags" "$timeout" "$requires" "$cmd"
  done
fi

SUITE_END=$(date +%s.%N)

# ---------------------------------------------------------------------------
# collect results (in selection order)
# ---------------------------------------------------------------------------
n_pass=0; n_fail=0; n_skip=0; n_err=0; sum_wall=0
declare -a ROWS=()
for rec in "${SELECTED[@]}"; do
  IFS="$US" read -r name rest <<< "$rec"
  if [ -f "$RESULTS/$name.result" ]; then
    ROWS+=("$(cat "$RESULTS/$name.result")")
  else
    ROWS+=("${name}${US}?${US}${US}error${US}99${US}0${US}0${US}0.0${US}0")
  fi
done
for row in "${ROWS[@]}"; do
  IFS="$US" read -r name tier tags outcome rc start end wall timed_out <<< "$row"
  case "$outcome" in
    pass)  n_pass=$((n_pass+1)) ;;
    fail)  n_fail=$((n_fail+1)) ;;
    skip)  n_skip=$((n_skip+1)) ;;
    error) n_err=$((n_err+1)) ;;
  esac
  sum_wall=$(awk "BEGIN{printf \"%.1f\", $sum_wall + $wall}")
done

suite_wall=$(awk "BEGIN{printf \"%.1f\", $SUITE_END-$SUITE_START}")
pool_wall=$(awk "BEGIN{printf \"%.1f\", $POOL_END-$POOL_START}")

# pool parallelism proof: serial-equiv sum + peak concurrency (interval sweep).
pool_sum=0; pool_peak=0
if [ "${#POOL[@]}" -gt 0 ]; then
  for rec in "${POOL[@]}"; do
    IFS="$US" read -r name rest <<< "$rec"
    [ -f "$RESULTS/$name.result" ] || continue
    w=$(awk -v S="$US" 'BEGIN{FS=S}{print $8}' "$RESULTS/$name.result")
    pool_sum=$(awk "BEGIN{printf \"%.1f\", $pool_sum + $w}")
  done
  pool_peak=$(cat "$RESULTS"/*.result 2>/dev/null | awk -v S="$US" '
    BEGIN{FS=S}
    $3 ~ /parallel-safe/ { print $6" s"; print $7" e" }' | sort -g | awk '
    { if($2=="s"){c++; if(c>mx)mx=c} else c-- } END{ print mx+0 }')
fi

# ---------------------------------------------------------------------------
# summary table
# ---------------------------------------------------------------------------
echo
echo "==================================================================="
echo " SUMMARY  suite=$SUITE"
echo "==================================================================="
printf '  %-22s %-4s %-7s %-7s %8s  %s\n' "NAME" "TIER" "PHASE" "RESULT" "TIME(s)" "TAGS"
printf '  %s\n' "---------------------------------------------------------------------------------"
for row in "${ROWS[@]}"; do
  IFS="$US" read -r name tier tags outcome rc start end wall timed_out <<< "$row"
  phase="serial"; has_tag "$tags" parallel-safe && phase="pool"
  extra=""; [ "$timed_out" = "1" ] && extra=" (TIMEOUT)"
  printf '  %-22s %-4s %-7s %-7s %8s  %s%s\n' \
    "$name" "$tier" "$phase" "$(echo "$outcome" | tr a-z A-Z)" "$wall" "$tags" "$extra"
done
printf '  %s\n' "---------------------------------------------------------------------------------"
printf '  totals: %d passed, %d failed, %d error, %d skipped  |  suite wall %ss\n' \
  "$n_pass" "$n_fail" "$n_err" "$n_skip" "$suite_wall"
if [ "${#POOL[@]}" -gt 0 ]; then
  spd=$(awk "BEGIN{ if($pool_wall>0) printf \"%.1f\", $pool_sum/$pool_wall; else print \"n/a\" }")
  printf '  parallel pool: %d tests in %ss wall (serial-equiv %ss; peak concurrency %s; %sx)\n' \
    "${#POOL[@]}" "$pool_wall" "$pool_sum" "$pool_peak" "$spd"
fi

# ---------------------------------------------------------------------------
# JSON results (shared TEST-1a schema:1 verdict/report shape)
# ---------------------------------------------------------------------------
verdict="pass"; exit_code=0
if [ "$n_fail" -gt 0 ]; then verdict="fail"; exit_code=1
elif [ "$n_err" -gt 0 ]; then verdict="error"; exit_code=2; fi

jesc() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }
JSON="$RESULTS/results.json"
{
  printf '{\n'
  printf '  "schema": 1,\n'
  printf '  "verdict": "%s",\n' "$verdict"
  printf '  "exit_code": %d,\n' "$exit_code"
  printf '  "suite": "%s",\n' "$SUITE"
  printf '  "jobs": %d,\n' "$JOBS"
  printf '  "duration_wall_s": %s,\n' "$suite_wall"
  printf '  "pool_wall_s": %s,\n' "$pool_wall"
  printf '  "pool_serial_equiv_s": %s,\n' "$pool_sum"
  printf '  "pool_peak_concurrency": %s,\n' "${pool_peak:-0}"
  printf '  "tests_total": %d,\n' "${#SELECTED[@]}"
  printf '  "tests_passed": %d,\n' "$n_pass"
  printf '  "tests_failed": %d,\n' "$n_fail"
  printf '  "tests_error": %d,\n' "$n_err"
  printf '  "tests_skipped": %d,\n' "$n_skip"
  printf '  "tests": [\n'
  last=$(( ${#ROWS[@]} - 1 )); i=0
  for row in "${ROWS[@]}"; do
    IFS="$US" read -r name tier tags outcome rc start end wall timed_out <<< "$row"
    phase="serial"; has_tag "$tags" parallel-safe && phase="pool"
    comma=","; [ "$i" -eq "$last" ] && comma=""
    printf '    {"name":"%s","tier":"%s","phase":"%s","tags":"%s","outcome":"%s","exit_code":%s,"duration_wall_s":%s,"timed_out":%s}%s\n' \
      "$(jesc "$name")" "$tier" "$phase" "$(jesc "$tags")" "$outcome" "$rc" "$wall" \
      "$( [ "$timed_out" = 1 ] && echo true || echo false )" "$comma"
    i=$((i+1))
  done
  printf '  ]\n}\n'
} > "$JSON"
echo "  JSON results: $JSON"

# ---------------------------------------------------------------------------
# advisory path hints (DOCS / echo -- not enforced)
# ---------------------------------------------------------------------------
echo
echo "  advisory (not enforced):"
echo "    * touched src/{lapic,park,vtsc}.rs ?  -> run T3 perf locally:  scripts/test.sh compare-stacks ff-demo-long"
echo "    * touched src/build.rs or a stack ?   -> run bake-repeat locally: scripts/test.sh bake-repeat-insert-trim"

if [ "$n_fail" -gt 0 ] || [ "$n_err" -gt 0 ]; then
  echo
  echo "  ---- failing/errored test logs (tail) ----"
  for row in "${ROWS[@]}"; do
    IFS="$US" read -r name tier tags outcome rc rest <<< "$row"
    case "$outcome" in fail|error)
      echo "  == $name ($outcome, rc=$rc) =="
      tail -12 "$RESULTS/$name.log" 2>/dev/null | sed 's/^/    /'
    ;; esac
  done
fi

echo
echo "VERDICT: $(echo "$verdict" | tr a-z A-Z) (exit $exit_code)  [$SUITE]"
exit "$exit_code"
