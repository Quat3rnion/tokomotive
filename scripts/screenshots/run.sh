#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
out="$root/docs/images"
if [[ ${1:-} == --out && $# == 2 ]]; then
    out=$2
elif [[ $# != 0 ]]; then
    echo "Usage: $0 [--out DIRECTORY]" >&2
    exit 2
fi
mkdir -p "$out"
export SHOTS_OUT_DIR
SHOTS_OUT_DIR=$(cd "$out" && pwd)
export SHOTS_PORT=${SHOTS_PORT:-18831} SHOTS_MOCK_PORT=${SHOTS_MOCK_PORT:-18832}
export SHOTS_PASSWORD=${SHOTS_PASSWORD:-demo-password-2026}
export SHOTS_BASE_URL="http://127.0.0.1:$SHOTS_PORT"
export SHOTS_MOCK_URL="http://127.0.0.1:$SHOTS_MOCK_PORT"
export PYTHONDONTWRITEBYTECODE=1
python3 - <<'PY'
import os
import socket
import requests
from PIL import Image
ports = [int(os.environ[name]) for name in ("SHOTS_PORT", "SHOTS_MOCK_PORT")]
assert len(set(ports)) == 2, "Proxy and mock ports must differ"
for port in ports:
    assert port not in (3000, 4095), "Ports 3000 and 4095 are reserved"
    with socket.socket() as sock:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind(("127.0.0.1", port))
PY
: "${CHROMIUM_PATH:?Set CHROMIUM_PATH to the Chromium executable}"
[[ -x $CHROMIUM_PATH ]] || { echo 'CHROMIUM_PATH is not executable' >&2; exit 1; }
node -e 'require("playwright-core")'
base=${SHOTS_WORKDIR:-/arboretum/tokomotive-screenshots}
mkdir -p "$base"
export SHOTS_WORKDIR
SHOTS_WORKDIR=$(mktemp -d "$base/run.XXXXXXXX")
export TMPDIR="$SHOTS_WORKDIR"
export SHOTS_CAPTURE_DIR="$SHOTS_WORKDIR/captures"
server_pid= mock_pid=
stop_server() {
    if [[ -n $server_pid ]]; then
        kill "$server_pid" 2>/dev/null || true
        for _ in {1..50}; do
            kill -0 "$server_pid" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
        server_pid=
    fi
}
cleanup() {
    local result=$?
    trap - EXIT INT TERM
    stop_server
    if [[ -n $mock_pid ]]; then
        kill "$mock_pid" 2>/dev/null || true
        wait "$mock_pid" 2>/dev/null || true
    fi
    if (( result != 0 )); then
        tail -n 30 "$SHOTS_WORKDIR"/*.log 2>/dev/null || true
    fi
    echo "Screenshot run data: $SHOTS_WORKDIR" >&2
    exit "$result"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
cd "$root"
if [[ -n ${SHOTS_BUILD_GATE:-} ]]; then
    "$SHOTS_BUILD_GATE" build cargo build --locked -j 4
else
    cargo build --locked -j 4
fi
target=${CARGO_TARGET_DIR:-$root/target}
[[ $target == /* ]] || target="$root/$target"
binary="$target/debug/tokomotive"
python3 "$root/scripts/screenshots/mock.py" >"$SHOTS_WORKDIR/mock.log" 2>&1 &
mock_pid=$!
start_server() {
    # No inherited production configuration, credentials, or bootstrap override.
    (
        unset TOKOMOTIVE_CONFIG TOKOMOTIVE_ADMIN_PASSWORD TOKOMOTIVE_METRICS_TOKEN
        unset TOKOMOTIVE_SECRET_KEY ANTHROPIC_API_KEY_2
        cd "$SHOTS_WORKDIR"
        exec "$binary" --listen "127.0.0.1:$SHOTS_PORT" --database "sqlite://$SHOTS_WORKDIR/toko.db"
    ) >>"$SHOTS_WORKDIR/server.log" 2>&1 &
    server_pid=$!
    python3 - "$server_pid" "$mock_pid" <<'PY'
import os
import sys
import time
import urllib.request
for attempt in range(150):
    for pid in sys.argv[1:]:
        os.kill(int(pid), 0)
    try:
        for url in (os.environ["SHOTS_BASE_URL"] + "/healthz", os.environ["SHOTS_MOCK_URL"]):
            with urllib.request.urlopen(url, timeout=1) as response:
                assert response.status == 200
        break
    except OSError:
        time.sleep(0.1)
else:
    raise SystemExit("Screenshot servers did not become ready")
PY
}
start_server
python3 "$root/scripts/screenshots/seed.py"
python3 "$root/scripts/screenshots/traffic.py"
stop_server
python3 "$root/scripts/screenshots/spread.py"
start_server
node "$root/scripts/screenshots/shoot.js"
python3 "$root/scripts/screenshots/quantize.py"
