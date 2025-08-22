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

# Accept EULA
echo 'eula=true' >"$SERVER_DIR/eula.txt"

# Start once to generate default configs
echo "initializing Paper server"
java -Xms64M -Xmx256M -jar "$JAR" nogui >/tmp/paper_init.log &
INIT_PID=$!
for i in $(seq 1 30); do
  if [ -f "$SERVER_DIR/config/paper-global.yml" ]; then
    break
  fi
  sleep 1
done
kill "$INIT_PID" 2>/dev/null || true
wait "$INIT_PID" 2>/dev/null || true

# Configure server to use Proxy Protocol
cat >"$SERVER_DIR/server.properties" <<'EOL'
server-port=25581
online-mode=false
motd=Test Server
EOL

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
java -Xms64M -Xmx256M -jar "$JAR" nogui >/tmp/paper.log &

# Wait for server port to open (max 60s)
echo "waiting for paper server to start"
for i in $(seq 1 60); do
  if (echo >"/dev/tcp/127.0.0.1/25581") >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
if ! (echo >"/dev/tcp/127.0.0.1/25581") >/dev/null 2>&1; then
  echo "paper server failed to start" >&2
  cat /tmp/paper.log >&2
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
