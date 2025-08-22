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

# Minimal server configuration with Proxy Protocol enabled
echo 'eula=true' >"$SERVER_DIR/eula.txt"
cat >"$SERVER_DIR/server.properties" <<'EOL'
server-port=25581
online-mode=false
motd=Test Server
EOL

mkdir -p "$SERVER_DIR/config"
cat >"$SERVER_DIR/config/paper-global.yml" <<'EOL'
proxies:
  proxy-protocol:
    enabled: true
    require: false
EOL

# Copy proxy config
cp "$DIR/config.yml" "$ROOT/config.yml"

# Start Paper server
java -Xms64M -Xmx1024M -jar "$JAR" nogui >/tmp/paper.log &
sleep 20

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
