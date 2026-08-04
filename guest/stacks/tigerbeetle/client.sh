#!/bin/sh
# deterministic-vmm TigerBeetle stack -- demo accounting client (closed world).
#
# Reuses the pinned TigerBeetle image's built-in REPL (no extra language
# toolchain, stays reproducible + closed-world). One `tigerbeetle repl
# --command=...` invocation per accounting op:
#
#   * once, at start: open two accounts A(id=1) and B(id=2) on ledger 700;
#   * then each cycle: submit a batch of 5 transfers of 100 each (A -> B),
#     read the two balances back, and log the cycle as a one-line narrative:
#         cycle 3: 5 transfers, 500 moved -> balance A(debits)=1500 B(credits)=1500
#   * sleep a virtual INTERVAL_SECONDS between cycles so the guest goes idle
#     (HLT) and dvmm fast-forwards the gap.
#
# Double-entry invariant: every transfer debits A and credits B by the same
# amount, so at all times A.debits_posted == B.credits_posted. The scenario
# asserts this from an exec (see tigerbeetle.yml).
#
# Addressing: same getent-resolve-to-IP dance as replica.sh (TigerBeetle's
# --addresses rejects hostnames); built in fixed replica order.
set -u

INTERVAL="${INTERVAL_SECONDS:-3600}"

resolve() { getent hosts "$1" | awk '{print $1; exit}'; }
log() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $*"; }

A0=""; A1=""; A2=""; i=0
while [ "$i" -lt 120 ]; do
  A0=$(resolve replica0 || true)
  A1=$(resolve replica1 || true)
  A2=$(resolve replica2 || true)
  [ -n "$A0" ] && [ -n "$A1" ] && [ -n "$A2" ] && break
  i=$((i + 1)); sleep 1
done
ADDRESSES="$A0:3000,$A1:3000,$A2:3000"
echo "client resolved addresses=$ADDRESSES"
echo "ADDRESSES=$ADDRESSES" > /tmp/tb-addresses

repl() { /tigerbeetle repl --cluster=0 --addresses="$ADDRESSES" --command="$1" 2>&1; }

# Open the two accounts (retry until the cluster answers and elects a primary).
i=0
while [ "$i" -lt 60 ]; do
  if repl 'create_accounts id=1 code=10 ledger=700 flags=history, id=2 code=10 ledger=700 flags=history;' | grep -q created; then
    log "opened accounts A=1 B=2 on ledger 700"
    break
  fi
  i=$((i + 1)); sleep 2
done

cycle=0
while true; do
  cycle=$((cycle + 1))
  base=$((cycle * 1000))

  # Build one batch of 5 comma-separated transfers (A -> B, 100 each).
  tr=""; moved=0; n=1
  while [ "$n" -le 5 ]; do
    id=$((base + n))
    sep=""; [ "$n" -gt 1 ] && sep=", "
    tr="${tr}${sep}id=$id debit_account_id=1 credit_account_id=2 amount=100 code=10 ledger=700"
    moved=$((moved + 100))
    n=$((n + 1))
  done

  if echo "$(repl "create_transfers ${tr};")" | grep -q created; then
    bal=$(repl 'lookup_accounts id=1, id=2;')
    A=$(echo "$bal" | grep -oE '"debits_posted": "[0-9]+"'  | head -1 | grep -oE '[0-9]+')
    B=$(echo "$bal" | grep -oE '"credits_posted": "[0-9]+"' | sed -n 2p | grep -oE '[0-9]+')
    log "cycle $cycle: 5 transfers, $moved moved -> balance A(debits)=${A:-?} B(credits)=${B:-?}"
  else
    log "cycle $cycle: transfers failed (cluster unavailable?), retrying next cycle"
  fi

  sleep "$INTERVAL"   # genuine virtual-interval sleep -> fast-forwarded when idle
done
