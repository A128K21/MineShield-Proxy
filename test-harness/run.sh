#!/usr/bin/env bash
set -euo pipefail

# Integration test harness for MineShield proxy against PaperMC (pinned jar via wget)

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
    # Accept modern Paper ready line and a fallback
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

# Download pinned Paper jar if needed (your provided URL)
if [ ! -f "$JAR" ]; then
  mkdir -p "$SERVER_DIR"
  echo "downloading pinned Paper 1.21.1 jar via wget"
  wget -O "$JAR" \
    "https://fill-data.papermc.io/v1/objects/39bd8c00b9e18de91dcabd3cc3dcfa5328685a53b7187a2f63280c22e2d287b9/paper-1.21.1-133.jar"
fi

# Minimal, fast server configuration
cat >"$SERVER_DIR/eula.txt" <<'EOL'
eula=true
EOL

cat >"$SERVER_DIR/server.properties" <<'EOL'
server-port=25581
online-mode=false
motd=Test Server
# speed up CI boot / TPS
level-type=flat
generate-structures=false
difficulty=peaceful
spawn-monsters=false
view-distance=2
simulation-distance=2
max-players=100
max-tick-time=-1
enable-status=true
EOL

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

# Boot Paper (single boot)
RUN_LOG=/tmp/paper.log
: >"$RUN_LOG"
(
  cd "$SERVER_DIR"
  # Larger heap reduces variance; capture stderr
  java -Xms256M -Xmx2G -jar ./paper.jar --nogui >"$RUN_LOG" 2>&1
) &

# Wait for ready state or dump logs
if ! wait_for_paper "$RUN_LOG"; then
  echo "Paper did not reach ready state; last 400 lines:" >&2
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

# Run multi-client test
echo "running multi-client test with $BOT_COUNT bots"
BOT_COUNT=$BOT_COUNT node "$DIR/multi_client.js"
echo "multi-client test completed"
