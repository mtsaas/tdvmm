#!/bin/sh
# deterministic-vmm Step 2a container self-test.
#
# Proves the guest is container-capable, fully offline (closed world):
#   1. create a podman bridge network ("appnet") via netavark, and
#   2. `podman run` a digest-pinned, pre-baked image on that network and
#      capture its stdout over the serial console.
#
# Prints machine-greppable markers so the host smoke test can assert success:
#   DVMM_NET_CREATE_OK / DVMM_NET_CREATE_FAIL
#   DVMM_PODMAN_RUN_OK / DVMM_PODMAN_RUN_FAIL
#   DVMM_CONTAINER_HELLO   (the container's own output)
#   DVMM_SELFTEST_PASS / DVMM_SELFTEST_FAIL
#
# The image reference (by digest) is baked in at build time.

set -u
IMAGE="$(cat /etc/dvmm-image-ref 2>/dev/null)"
NET=appnet
HELLO=DVMM_CONTAINER_HELLO

fail() { echo "DVMM_SELFTEST_FAIL: $*"; exit 1; }

echo "[selftest] podman version:"
podman version 2>&1 | sed 's/^/[selftest] /' | head -6
echo "[selftest] baked image: $IMAGE"

# 1. bridge network -----------------------------------------------------------
# NOTE: do not pipe podman into sed for the success test — that would report
# sed's exit status, not podman's.
echo "[selftest] creating podman network '$NET' (netavark)"
if podman network create "$NET" > /tmp/net.out 2>&1; then
  sed 's/^/[selftest][net] /' /tmp/net.out
  echo "DVMM_NET_CREATE_OK"
else
  sed 's/^/[selftest][net] /' /tmp/net.out
  echo "DVMM_NET_CREATE_FAIL"
  fail "podman network create"
fi

# 2. run the pre-baked, digest-pinned image on that network -------------------
# --pull=never enforces zero runtime networking (image must already be local).
echo "[selftest] podman run --network $NET (pull=never) ..."
OUT="$(podman run --rm --network "$NET" --pull=never "$IMAGE" sh -c "echo $HELLO" 2>/tmp/selftest.err)"
RC=$?
sed 's/^/[selftest][run-stderr] /' /tmp/selftest.err 2>/dev/null

if [ $RC -eq 0 ] && [ "$OUT" = "$HELLO" ]; then
  echo "$OUT"
  echo "DVMM_PODMAN_RUN_OK"
else
  echo "podman run rc=$RC out=[$OUT]"
  echo "DVMM_PODMAN_RUN_FAIL"
  fail "podman run"
fi

echo "DVMM_SELFTEST_PASS"
exit 0
