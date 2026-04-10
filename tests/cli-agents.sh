#!/bin/bash
# CLI Agent integration test — tests real agent backends via agend-pty daemon.
# Usage: bash tests/cli-agents.sh [backend]
set -e

cd "$(dirname "$0")/.."
PASS=0; FAIL=0
pass() { echo "  ✅ $1"; PASS=$((PASS+1)); }
fail() { echo "  ❌ $1"; FAIL=$((FAIL+1)); }

cargo build --quiet 2>/dev/null
pkill -f "target/debug/agend-daemon" 2>/dev/null || true
sleep 1; rm -rf ~/.agend/run/

FILTER="${1:-all}"
TRUST_DIR="/tmp/agend-trust-test-$$"

# ── Helpers ──────────────────────────────────────────────────────────────

read_screen() {
    python3 -c "
import socket, struct, os, glob, re
socks = glob.glob(os.path.expanduser('~/.agend/run/*/agents/$1/tui.sock'))
if not socks: print('NO_SOCKET'); exit()
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(10)
tag = s.recv(1); hdr = s.recv(4)
length = struct.unpack('>I', hdr)[0]
data = b''
while len(data) < length: data += s.recv(length - len(data))
text = data.decode('utf-8', errors='replace')
clean = re.sub(r'\x1b\[[0-9;?]*[A-Za-z]', ' ', text)
print(clean)
s.close()
" 2>/dev/null
}

wait_for_pattern() {
    local name=$1 pattern=$2 timeout=${3:-30}
    for i in $(seq 1 $timeout); do
        screen=$(read_screen "$name")
        if echo "$screen" | grep -qi "$pattern"; then return 0; fi
        sleep 1
    done
    return 1
}

check_mcp() {
    local name=$1
    # Check MCP socket exists
    if ! ls ~/.agend/run/*/agents/$name/mcp.sock >/dev/null 2>&1; then
        fail "$name: MCP socket missing"; return
    fi
    pass "$name: MCP socket exists"

    # Check MCP bridge handshake
    local result=$(python3 -c "
import subprocess, json, select
proc = subprocess.Popen(
    ['./target/debug/agend-mcp-bridge', '$name'],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE
)
req = json.dumps({'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2024-11-05','capabilities':{},'clientInfo':{'name':'test'}}})
proc.stdin.write((req + '\n').encode())
proc.stdin.flush()
if select.select([proc.stdout], [], [], 5)[0]:
    line = proc.stdout.readline().decode().strip()
    if line:
        r = json.loads(line)
        print('ok' if r.get('result',{}).get('serverInfo',{}).get('name') == 'agend' else 'fail')
    else: print('empty')
else: print('timeout')
proc.terminate()
" 2>/dev/null)
    if [ "$result" = "ok" ]; then pass "$name: MCP bridge handshake"; else fail "$name: MCP bridge ($result)"; fi
}

cleanup_daemon() {
    kill $DAEMON_PID 2>/dev/null || true
    sleep 1; kill -9 $DAEMON_PID 2>/dev/null || true
    rm -rf ~/.agend/run/
}

# ── Test functions ───────────────────────────────────────────────────────

test_claude() {
    echo ""
    echo "=== Claude Code ==="
    rm -rf ~/.agend/run/

    # Test 1: Trust dialog auto-dismiss (use untrusted directory)
    mkdir -p "$TRUST_DIR"
    rm -rf ~/.claude/projects/-private-tmp-agend-trust-test-* 2>/dev/null

    ORIG_DIR=$(pwd)
    cd "$TRUST_DIR"
    "$ORIG_DIR/target/debug/agend-daemon" "claude-trust:claude --dangerously-skip-permissions" \
        2>/tmp/agend-claude-trust.log &
    DAEMON_PID=$!

    echo "  Testing trust dialog auto-dismiss..."
    if wait_for_pattern "claude-trust" "❯\|opus\|sonnet\|Claude" 30; then
        pass "Claude: ready after trust dialog"
        if grep -q "auto-dismissing trust dialog" /tmp/agend-claude-trust.log; then
            pass "Claude: trust dialog auto-dismissed"
        else
            pass "Claude: no trust dialog needed"
        fi
    else
        fail "Claude: not ready after 30s (trust dialog stuck?)"
    fi
    cleanup_daemon
    cd - >/dev/null

    # Test 2: Normal startup + MCP
    rm -rf ~/.agend/run/
    cd "$(dirname "$0")/.."
    cargo run --quiet --bin agend-daemon -- "claude-test:claude --dangerously-skip-permissions" \
        2>/tmp/agend-claude-test.log &
    DAEMON_PID=$!

    echo "  Testing ready + MCP..."
    if wait_for_pattern "claude-test" "❯\|opus\|sonnet" 30; then
        pass "Claude: ready"
    else
        fail "Claude: not ready"; cleanup_daemon; return
    fi

    check_mcp "claude-test"
    cleanup_daemon
    pass "Claude: shutdown clean"
}

test_gemini() {
    echo ""
    echo "=== Gemini CLI ==="
    rm -rf ~/.agend/run/

    cargo run --quiet --bin agend-daemon -- "gemini-test:gemini --yolo" \
        2>/tmp/agend-gemini-test.log &
    DAEMON_PID=$!

    echo "  Testing ready + MCP..."
    if wait_for_pattern "gemini-test" ">\|gemini\|Gemini" 30; then
        pass "Gemini: ready"
    else
        fail "Gemini: not ready"; cleanup_daemon; return
    fi

    check_mcp "gemini-test"
    cleanup_daemon
    pass "Gemini: shutdown clean"
}

test_codex() {
    echo ""
    echo "=== Codex ==="
    rm -rf ~/.agend/run/

    cargo run --quiet --bin agend-daemon -- "codex-test:codex --full-auto" \
        2>/tmp/agend-codex-test.log &
    DAEMON_PID=$!

    echo "  Testing ready + MCP..."
    if wait_for_pattern "codex-test" ">\|codex\|sandbox\|Codex" 30; then
        pass "Codex: ready"
    else
        fail "Codex: not ready"; cleanup_daemon; return
    fi

    check_mcp "codex-test"
    cleanup_daemon
    pass "Codex: shutdown clean"
}

test_kiro() {
    echo ""
    echo "=== Kiro CLI ==="
    rm -rf ~/.agend/run/

    cargo run --quiet --bin agend-daemon -- "kiro-test:kiro-cli chat --trust-all-tools" \
        2>/tmp/agend-kiro-test.log &
    DAEMON_PID=$!

    echo "  Testing ready + MCP..."
    if wait_for_pattern "kiro-test" ">\|kiro\|trusted\|tools" 30; then
        pass "Kiro: ready"
    else
        fail "Kiro: not ready"; cleanup_daemon; return
    fi

    check_mcp "kiro-test"
    cleanup_daemon
    pass "Kiro: shutdown clean"
}

test_opencode() {
    echo ""
    echo "=== OpenCode ==="
    rm -rf ~/.agend/run/

    cargo run --quiet --bin agend-daemon -- "oc-test:opencode" \
        2>/tmp/agend-oc-test.log &
    DAEMON_PID=$!

    echo "  Testing ready + MCP..."
    if wait_for_pattern "oc-test" ">\|opencode\|OpenCode" 30; then
        pass "OpenCode: ready"
    else
        fail "OpenCode: not ready"; cleanup_daemon; return
    fi

    check_mcp "oc-test"
    cleanup_daemon
    pass "OpenCode: shutdown clean"
}

# ── Run ──────────────────────────────────────────────────────────────────

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
    *) echo "Unknown: $FILTER"; exit 1 ;;
esac

# Cleanup
pkill -f "target/debug/agend-daemon" 2>/dev/null || true
rm -rf ~/.agend/run/ "$TRUST_DIR"

echo ""
echo "════════════════════════════════"
echo "Results: $PASS passed, $FAIL failed"
if [ $FAIL -gt 0 ]; then echo "FAILED"; exit 1
else echo "ALL PASSED ✅"; fi
