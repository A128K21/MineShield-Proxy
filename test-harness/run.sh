#!/usr/bin/env bash
set -euo pipefail

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
  for _ in {1..120}; do
    if grep -Fq 'For help, type "help"' "$log_file" 2>/dev/null; then
      return 0
    fi
    sleep 1
  done
  echo "Paper server failed to start within timeout" >&2
  return 1
}

# Build proxy
cargo build >/tmp/proxy_build.log

# Install npm dependencies
npm --prefix "$DIR" install >/tmp/npm_install.log

# Download PaperMC if needed
if [ ! -f "$JAR" ]; then
  mkdir -p "$SERVER_DIR"
  echo "downloading PaperMC 1.21.1"
  USER_AGENT="mineshield-test/1.0 (+https://example.com/contact)"
  API="https://fill.papermc.io/v3/projects/paper/versions/1.21.1/builds"
  URL=$(curl -fsSL -H "User-Agent: ${USER_AGENT}" "${API}" \
    | jq -r 'first(.[] | select(.channel=="STABLE") | .downloads."server:default".url) // "null"')
  if [[ "$URL" == "null" ]]; then
    echo "No stable build found for 1.21.1." >&2
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

# Start once to generate configuration files
INIT_LOG=/tmp/paper_init.log
: >"$INIT_LOG"
java -Xms64M -Xmx1024M -jar "$JAR" nogui >"$INIT_LOG" &
INIT_PID=$!
wait_for_paper "$INIT_LOG"
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

# Copy proxy config
cp "$DIR/config.yml" "$ROOT/config.yml"

# Start Paper server
RUN_LOG=/tmp/paper.log
: >"$RUN_LOG"
java -Xms64M -Xmx1024M -jar "$JAR" nogui >"$RUN_LOG" &
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
