#!/bin/bash
set -euo pipefail

APP_PATH="${1:?usage: macos-smoke.sh /path/to/DeepSeek Harness.app}"
LOG_DIR="${RUNNER_TEMP:-/tmp}/dsh-macos-smoke"
APP_BIN="$APP_PATH/Contents/MacOS/dsh-desktop"
BUNDLED_NODE="$APP_PATH/Contents/Resources/backend/node"
BUNDLED_BIN="$APP_PATH/Contents/Resources/backend/node_modules/@deepseek-ai/dsh/lib/bin.js"
APP_PID=""
BACKEND_PID=""

mkdir -p "$LOG_DIR"

cleanup() {
  ps -axo pid=,ppid=,command= > "$LOG_DIR/processes.txt" || true
  if [[ -n "$BACKEND_PID" ]] && kill -0 "$BACKEND_PID" 2>/dev/null; then
    kill "$BACKEND_PID" 2>/dev/null || true
  fi
  if [[ -n "$APP_PID" ]] && kill -0 "$APP_PID" 2>/dev/null; then
    kill "$APP_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

for required in "$APP_BIN" "$BUNDLED_NODE" "$BUNDLED_BIN"; do
  [[ -e "$required" ]] || { echo "missing bundled file: $required" >&2; exit 1; }
done

"$APP_BIN" > "$LOG_DIR/app-stdout.txt" 2> "$LOG_DIR/app-stderr.txt" &
APP_PID=$!
deadline=$((SECONDS + 60))

while (( SECONDS < deadline )); do
  kill -0 "$APP_PID" 2>/dev/null || { echo "desktop app exited during startup" >&2; exit 1; }
  backend_line=$(ps -axo pid=,ppid=,command= | awk -v parent="$APP_PID" -v node="$BUNDLED_NODE" '
    $2 == parent && index($0, node) && /@deepseek-ai\/dsh\/lib\/bin\.js web/ { print; exit }
  ')
  if [[ -n "$backend_line" ]]; then
    BACKEND_PID=$(awk '{print $1}' <<< "$backend_line")
    break
  fi
  sleep 1
done

[[ -n "$BACKEND_PID" ]] || { echo "bundled backend did not start" >&2; exit 1; }
port=$(sed -E 's/.*--port ([0-9]+).*/\1/' <<< "$backend_line")
[[ "$port" =~ ^[0-9]+$ ]] || { echo "could not parse backend port: $backend_line" >&2; exit 1; }

url="http://127.0.0.1:$port"
deadline=$((SECONDS + 60))
while (( SECONDS < deadline )); do
  kill -0 "$APP_PID" 2>/dev/null || { echo "desktop app exited before ready" >&2; exit 1; }
  kill -0 "$BACKEND_PID" 2>/dev/null || { echo "bundled backend exited before ready" >&2; exit 1; }
  if curl -fsS --max-time 3 "$url" -o "$LOG_DIR/response.html" && [[ -s "$LOG_DIR/response.html" ]]; then
    break
  fi
  sleep 1
done

[[ -s "$LOG_DIR/response.html" ]] || { echo "backend did not respond at $url" >&2; exit 1; }
sleep 10
kill -0 "$APP_PID" 2>/dev/null || { echo "desktop app exited during soak" >&2; exit 1; }
kill -0 "$BACKEND_PID" 2>/dev/null || { echo "bundled backend exited during soak" >&2; exit 1; }
echo "Application stayed alive and bundled backend responded at $url" | tee "$LOG_DIR/result.txt"
