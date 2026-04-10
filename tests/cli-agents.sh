#!/bin/bash
# CLI Agent integration test — tests real agent backends via agend-pty daemon.
# Usage: bash tests/cli-agents.sh [backend]
#   bash tests/cli-agents.sh claude    # test Claude only
#   bash tests/cli-agents.sh           # test all available
set -e

cd "$(dirname "$0")/.."
PASS=0; FAIL=0
pass() { echo "  ✅ $1"; PASS=$((PASS+1)); }
fail() { echo "  ❌ $1"; FAIL=$((FAIL+1)); }

cargo build --quiet 2>/dev/null
pkill -f "target/debug/agend-daemon" 2>/dev/null || true
sleep 1; rm -rf ~/.agend/run/

FILTER="${1:-all}"

# Helper: connect to TUI socket, read screen dump, return text
read_screen() {
    local name=$1
    python3 -c "
import socket, struct, os, glob, re
socks = glob.glob(os.path.expanduser('~/.agend/run/*/agents/${name}/tui.sock'))
if not socks: print('NO_SOCKET'); exit()
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(10)
tag = s.recv(1); hdr = s.recv(4)
length = struct.unpack('>I', hdr)[0]
data = b''
while len(data) < length: data += s.recv(length - len(data))
# Strip ANSI
text = data.decode('utf-8', errors='replace')
clean = re.sub(r'\x1b\[[0-9;?]*[A-Za-z]', ' ', text)
print(clean)
s.close()
" 2>/dev/null
}

# Helper: send input to agent via TUI socket
send_input() {
    local name=$1 input=$2
    python3 -c "
import socket, struct, os, glob
socks = glob.glob(os.path.expanduser('~/.agend/run/*/agents/${name}/tui.sock'))
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(5)
# Drain screen dump
tag = s.recv(1); hdr = s.recv(4)
length = struct.unpack('>I', hdr)[0]
while length > 0:
    chunk = s.recv(min(8192, length))
    length -= len(chunk)
# Send input
data = b'${input}'
s.send(b'\x00' + struct.pack('>I', len(data)) + data)
s.close()
" 2>/dev/null
}

# Helper: wait for pattern in screen dump (with timeout)
wait_for_pattern() {
    local name=$1 pattern=$2 timeout=${3:-30}
    for i in $(seq 1 $timeout); do
        screen=$(read_screen "$name")
        if echo "$screen" | grep -qi "$pattern"; then
            return 0
        fi
        sleep 1
    done
    return 1
}

# ═══════════════════════════════════════════════════════════════════════
# Claude Code
# ═══════════════════════════════════════════════════════════════════════
test_claude() {
    echo ""
    echo "=== Claude Code ==="
    
    cargo run --quiet --bin agend-daemon -- "claude-test:claude --dangerously-skip-permissions" 2>/tmp/agend-claude-test.log &
    DAEMON_PID=$!
    
    # Wait for trust dialog auto-dismiss + ready
    echo "  Waiting for Claude to be ready (up to 30s)..."
    if wait_for_pattern "claude-test" "❯\|Type your\|opus\|sonnet" 30; then
        pass "Claude ready"
    else
        fail "Claude not ready after 30s"
        cat /tmp/agend-claude-test.log | grep -i "trust\|error\|dismiss" | tail -5
        kill $DAEMON_PID 2>/dev/null; wait $DAEMON_PID 2>/dev/null
        return
    fi
    
    # Check auto-trust was dismissed
    if grep -q "auto-dismissing trust dialog" /tmp/agend-claude-test.log; then
        pass "Trust dialog auto-dismissed"
    else
        pass "No trust dialog (already trusted)"
    fi
    
    # Check MCP bridge connected
    if ls ~/.agend/run/*/agents/claude-test/mcp.sock >/dev/null 2>&1; then
        pass "MCP socket exists"
    else
        fail "MCP socket missing"
    fi
    
    # Test MCP tools via bridge
    MCP_RESULT=$(python3 -c "
import subprocess, json, time
proc = subprocess.Popen(
    ['./target/debug/agend-mcp-bridge', 'claude-test'],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE
)
req = json.dumps({'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2024-11-05','capabilities':{},'clientInfo':{'name':'test'}}})
proc.stdin.write((req + '\n').encode())
proc.stdin.flush()
import select
if select.select([proc.stdout], [], [], 5)[0]:
    line = proc.stdout.readline().decode().strip()
    r = json.loads(line)
    print('ok' if r.get('result',{}).get('serverInfo',{}).get('name') == 'agend' else 'fail')
else:
    print('timeout')
proc.terminate()
" 2>/dev/null)
    if [ "$MCP_RESULT" = "ok" ]; then pass "MCP bridge handshake"; else fail "MCP bridge: $MCP_RESULT"; fi
    
    # Shutdown
    kill $DAEMON_PID 2>/dev/null || true
    sleep 1
    kill -9 $DAEMON_PID 2>/dev/null || true
    rm -rf ~/.agend/run/
    pass "Claude shutdown clean"
}

# ═══════════════════════════════════════════════════════════════════════
# Gemini CLI
# ═══════════════════════════════════════════════════════════════════════
test_gemini() {
    echo ""
    echo "=== Gemini CLI ==="
    
    rm -rf ~/.agend/run/
    cargo run --quiet --bin agend-daemon -- "gemini-test:gemini --yolo" 2>/tmp/agend-gemini-test.log &
    DAEMON_PID=$!
    
    echo "  Waiting for Gemini to be ready (up to 30s)..."
    if wait_for_pattern "gemini-test" ">\|gemini\|ready" 30; then
        pass "Gemini ready"
    else
        fail "Gemini not ready after 30s"
        kill $DAEMON_PID 2>/dev/null; wait $DAEMON_PID 2>/dev/null
        return
    fi
    
    kill $DAEMON_PID 2>/dev/null || true
    sleep 1
    kill -9 $DAEMON_PID 2>/dev/null || true
    rm -rf ~/.agend/run/
    pass "Gemini shutdown clean"
}

# ═══════════════════════════════════════════════════════════════════════
# Codex
# ═══════════════════════════════════════════════════════════════════════
test_codex() {
    echo ""
    echo "=== Codex ==="

    rm -rf ~/.agend/run/
    cargo run --quiet --bin agend-daemon -- "codex-test:codex --full-auto" 2>/tmp/agend-codex-test.log &
    DAEMON_PID=$!

    echo "  Waiting for Codex to be ready (up to 30s)..."
    if wait_for_pattern "codex-test" ">\|codex\|sandbox" 30; then
        pass "Codex ready"
    else
        fail "Codex not ready after 30s"
        kill $DAEMON_PID 2>/dev/null || true; kill -9 $DAEMON_PID 2>/dev/null || true
        rm -rf ~/.agend/run/; return
    fi

    kill $DAEMON_PID 2>/dev/null || true
    sleep 1; kill -9 $DAEMON_PID 2>/dev/null || true
    rm -rf ~/.agend/run/
    pass "Codex shutdown clean"
}

# ═══════════════════════════════════════════════════════════════════════
# Kiro CLI
# ═══════════════════════════════════════════════════════════════════════
test_kiro() {
    echo ""
    echo "=== Kiro CLI ==="

    rm -rf ~/.agend/run/
    cargo run --quiet --bin agend-daemon -- "kiro-test:kiro-cli chat --trust-all-tools" 2>/tmp/agend-kiro-test.log &
    DAEMON_PID=$!

    echo "  Waiting for Kiro to be ready (up to 30s)..."
    if wait_for_pattern "kiro-test" ">\|kiro\|trusted\|tools" 30; then
        pass "Kiro ready"
    else
        fail "Kiro not ready after 30s"
        kill $DAEMON_PID 2>/dev/null || true; kill -9 $DAEMON_PID 2>/dev/null || true
        rm -rf ~/.agend/run/; return
    fi

    kill $DAEMON_PID 2>/dev/null || true
    sleep 1; kill -9 $DAEMON_PID 2>/dev/null || true
    rm -rf ~/.agend/run/
    pass "Kiro shutdown clean"
}

# ═══════════════════════════════════════════════════════════════════════
# OpenCode
# ═══════════════════════════════════════════════════════════════════════
test_opencode() {
    echo ""
    echo "=== OpenCode ==="

    rm -rf ~/.agend/run/
    cargo run --quiet --bin agend-daemon -- "oc-test:opencode" 2>/tmp/agend-oc-test.log &
    DAEMON_PID=$!

    echo "  Waiting for OpenCode to be ready (up to 30s)..."
    if wait_for_pattern "oc-test" ">\|opencode\|ready" 30; then
        pass "OpenCode ready"
    else
        fail "OpenCode not ready after 30s"
        kill $DAEMON_PID 2>/dev/null || true; kill -9 $DAEMON_PID 2>/dev/null || true
        rm -rf ~/.agend/run/; return
    fi

    kill $DAEMON_PID 2>/dev/null || true
    sleep 1; kill -9 $DAEMON_PID 2>/dev/null || true
    rm -rf ~/.agend/run/
    pass "OpenCode shutdown clean"
}

# ═══════════════════════════════════════════════════════════════════════
# Run selected tests
# ═══════════════════════════════════════════════════════════════════════

echo "agend-pty CLI Agent Tests"
echo "========================="

case "$FILTER" in
    claude) test_claude ;;
    gemini) test_gemini ;;
    codex) test_codex ;;
    kiro) test_kiro ;;
    opencode) test_opencode ;;
    all)
        which claude >/dev/null 2>&1 && test_claude
        which gemini >/dev/null 2>&1 && test_gemini
        which codex >/dev/null 2>&1 && test_codex
        which kiro-cli >/dev/null 2>&1 && test_kiro
        which opencode >/dev/null 2>&1 && test_opencode
        ;;
    *) echo "Unknown backend: $FILTER"; exit 1 ;;
esac

# Cleanup
pkill -f "target/debug/agend-daemon" 2>/dev/null || true
rm -rf ~/.agend/run/

echo ""
echo "════════════════════════════════"
echo "Results: $PASS passed, $FAIL failed"
if [ $FAIL -gt 0 ]; then echo "FAILED"; exit 1
else echo "ALL PASSED ✅"; fi
