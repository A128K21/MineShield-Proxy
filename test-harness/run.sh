#!/usr/bin/env bash
set -euo pipefail

# Integration test harness for MineShield proxy against PaperMC
# Notes:
# - Uses official Paper v2 API to fetch the latest build of 1.21.1
# - Captures both stdout and stderr to logs
# - Waits up to 240s for startup and matches the modern "Done (...)! For help, type \"help\"" line
# - Allocates a bit more heap to reduce CI startup variance

DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$DIR/.."
SERVER_DIR="$DIR/paper-server"
JAR="$SERVER_DIR/paper.jar"

cleanup() {
  # Kill any children of this script and remove copied config
  pkill -P $$ || true
  rm -f "$ROOT/config.yml"
}
trap cleanup EXIT

BOT_COUNT=${BOT_COUNT:-50}

wait_for_paper() {
  local log_file="$1"
  # Look for the canonical "Done (...)! For help, type "help"" line, but also accept the fallback.
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

# Download PaperMC if needed (latest build for 1.21.1)
if [ ! -f "$JAR" ]; then
  mkdir -p "$SERVER_DIR"
  echo "downloading PaperMC 1.21.1 (latest build)"
  USER_AGENT="mineshield-test/1.0 (+https://example.com/contact)"
  API="https://api.papermc.io/v2/projects/paper/versions/1.21.1/builds"
  # Pick the highest build number and construct the download URL for the application jar
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

# Prepare minimal server configuration and generate defaults
echo 'eula=true' >"$SERVER_DIR/eula.txt"
cat >"$SERVER_DIR/server.properties" <<'EOL'
server-port=25581
online-mode=false
motd=Test Server
EOL

# First start to generate config files
INIT_LOG=/tmp/paper_init.log
: >"$INIT_LOG"
(java -Xms256M -Xmx2G -jar "$JAR" --nogui >"$INIT_LOG" 2>&1) &
INIT_PID=$!
wait_for_paper "$INIT_LOG" || { echo "Initial Paper boot failed"; exit 1; }
kill "$INIT_PID" 2>/dev/null || true
wait "$INIT_PID" 2>/dev/null || true

# Overwrite Paper configuration enabling Proxy Protocol
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

# Copy proxy config to project root (where the binary reads it from)
cp "$DIR/config.yml" "$ROOT/config.yml"

# Start Paper server for the actual test
RUN_LOG=/tmp/paper.log
: >"$RUN_LOG"
(java -Xms256M -Xmx2G -jar "$JAR" --nogui >"$RUN_LOG" 2>&1) &
wait_for_paper "$RUN_LOG"

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
