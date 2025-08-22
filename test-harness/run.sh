#!/usr/bin/env bash
set -euo pipefail

# Integration test harness for MineShield proxy against PaperMC

DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$DIR/.."
SERVER_DIR="$DIR/paper-server"
JAR="$SERVER_DIR/paper.jar"

cleanup() {
  pkill -P $$ || true
  rm -f "$ROOT/config.yml"
}
trap cleanup EXIT

BOT_COUNT=${BOT_COUNT:-50}

wait_for_paper() {
  local log_file="$1"
  for _ in {1..240}; do
    if grep -Eq 'Done \([0-9.]+s\)! For help, type "help"|For help, type "help"' "$log_file" 2>/dev/null; then
      return 0
    fi
    sleep 1
  done
  echo "Paper server failed to start within timeout" >&2
  return 1
}

# Build proxy
cargo build >/tmp/proxy_build.log 2>&1

# Install npm dependencies for the test harness
npm --prefix "$DIR" install >/tmp/npm_install.log 2>&1

# Download latest Paper build for 1.21.1 if needed (official v2 API)
if [ ! -f "$JAR" ]; then
  mkdir -p "$SERVER_DIR"
  echo "downloading PaperMC 1.21.1 (latest build)"
  USER_AGENT="mineshield-test/1.0 (+https://example.com/contact)"
  API="https://api.papermc.io/v2/projects/paper/versions/1.21.1/builds"
  URL=$(
    curl -fsSL -H "User-Agent: ${USER_AGENT}" "$API" \
      | jq -r '.builds | sort_by(.build) | .[-1] as $b
               | "https://api.papermc.io/v2/projects/paper/versions/1.21.1/builds/\($b.build)/downloads/\($b.downloads.application.name)"'
  )
  if [[ -z "${URL}" || "${URL}" == "null" ]]; then
    echo "Unable to resolve Paper download URL for 1.21.1." >&2
    exit 1
  fi
  curl -fsSL -H "User-Agent: ${USER_AGENT}" -o "$JAR" "$URL"
fi

# Minimal server configuration
echo 'eula=true' >"$SERVER_DIR/eula.txt"
cat >"$SERVER_DIR/server.properties" <<'EOL'
server-port=25581
online-mode=false
motd=Test Server
EOL

# First start to generate configs
INIT_LOG=/tmp/paper_init.log
: >"$INIT_LOG"
( cd "$SERVER_DIR" && java -Xms256M -Xmx2G -jar ./paper.jar --nogui >"$INIT_LOG" 2>&1 ) &
INIT_PID=$!
if ! wait_for_paper "$INIT_LOG"; then
  echo "Initial Paper boot failed"
  sed -n '1,200p' "$INIT_LOG" >&2 || true
  exit 1
fi
kill "$INIT_PID" 2>/dev/null || true
wait "$INIT_PID" 2>/dev/null || true

# Enable Proxy Protocol in Paper
mkdir -p "$SERVER_DIR/config"
cat >"$SERVER_DIR/config/paper-global.yml" <<'EOL'
proxies:
  bungee-cord:
    online-mode: true
  proxy-protocol: true
  velocity:
    enabled: false
    online-mode: true
    secret: ''
EOL

# Copy proxy config for the binary
cp "$DIR/config.yml" "$ROOT/config.yml"

# Start Paper for tests
RUN_LOG=/tmp/paper.log
: >"$RUN_LOG"
( cd "$SERVER_DIR" && java -Xms256M -Xmx2G -jar ./paper.jar --nogui >"$RUN_LOG" 2>&1 ) &
if ! wait_for_paper "$RUN_LOG"; then
  echo "Paper failed to reach ready state"
  tail -n 400 "$RUN_LOG" >&2 || true
  exit 1
fi

# Start proxy
"$ROOT/target/debug/mineshieldv2-proxy" &
PROXY_PID=$!
sleep 2

# Ping proxy
echo "running ping test"
node "$DIR/ping_test.js"

# Run clients
echo "running multi-client test with $BOT_COUNT bots"
BOT_COUNT=$BOT_COUNT node "$DIR/multi_client.js"
echo "multi-client test completed"
