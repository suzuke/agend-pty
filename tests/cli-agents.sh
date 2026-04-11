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
export AGEND_TEST_PASSPHRASE="招弟"

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

# Send input to agent PTY
send_input() {
    local name=$1; shift
    local input="$*"
    python3 - "$name" "$input" <<'PYEOF'
import socket, struct, os, glob, sys
name, text = sys.argv[1], sys.argv[2]
socks = glob.glob(os.path.expanduser(f'~/.agend/run/*/agents/{name}/tui.sock'))
if not socks: exit(1)
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(5)
tag = s.recv(1); hdr = s.recv(4); length = struct.unpack('>I', hdr)[0]
while length > 0:
    chunk = s.recv(min(8192, length)); length -= len(chunk)
data = text.encode()
s.send(b'\x00' + struct.pack('>I', len(data)) + data)
s.close()
PYEOF
}

# Check instructions file exists at correct backend-specific location
check_instructions() {
    local name=$1 cmd=$2 workdir=$3
    local found=0
    if echo "$cmd" | grep -qi claude && [ -f "$workdir/.claude/rules/agend.md" ]; then found=1; fi
    if echo "$cmd" | grep -qi gemini && [ -f "$workdir/GEMINI.md" ]; then found=1; fi
    if echo "$cmd" | grep -qi codex && [ -f "$workdir/AGENTS.md" ]; then found=1; fi
    if echo "$cmd" | grep -qi kiro && [ -f "$workdir/AGENTS.md" ]; then found=1; fi
    if echo "$cmd" | grep -qi opencode && [ -f "$workdir/fleet-instructions.md" ]; then found=1; fi
    if [ $found -eq 1 ]; then pass "$name: instructions file"; else fail "$name: instructions file missing"; fi
}

# Check reconnect: disconnect, reconnect, screen still has content
check_reconnect() {
    local name=$1
    local result=$(python3 -c "
import socket, struct, os, glob, re, time
socks = glob.glob(os.path.expanduser('~/.agend/run/*/agents/$name/tui.sock'))
if not socks: print('no_socket'); exit()
# Connect 1: read screen
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(5)
tag = s.recv(1); hdr = s.recv(4); length = struct.unpack('>I', hdr)[0]
data1 = b''
while len(data1) < length: data1 += s.recv(length - len(data1))
s.close()
time.sleep(0.5)
# Connect 2: should get screen dump again
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(5)
tag = s.recv(1); hdr = s.recv(4); length = struct.unpack('>I', hdr)[0]
data2 = b''
while len(data2) < length: data2 += s.recv(length - len(data2))
s.close()
print('ok' if len(data2) > 50 else 'fail:empty')
" 2>/dev/null)
    if [ "$result" = "ok" ]; then pass "$name: reconnect + screen dump"; else fail "$name: reconnect ($result)"; fi
}

# Check resize
check_resize() {
    local name=$1
    local result=$(python3 -c "
import socket, struct, os, glob
socks = glob.glob(os.path.expanduser('~/.agend/run/*/agents/$name/tui.sock'))
if not socks: print('no_socket'); exit()
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(5)
# Drain screen dump
tag = s.recv(1); hdr = s.recv(4); length = struct.unpack('>I', hdr)[0]
while length > 0:
    chunk = s.recv(min(8192, length)); length -= len(chunk)
# Send resize (tag=1, cols=100, rows=30)
data = struct.pack('>HH', 100, 30)
s.send(b'\x01' + struct.pack('>I', len(data)) + data)
import time; time.sleep(0.5)
print('ok')
s.close()
" 2>/dev/null)
    if [ "$result" = "ok" ]; then pass "$name: resize"; else fail "$name: resize ($result)"; fi
}

# Inject test passphrase into instructions file and verify agent knows it
PASSPHRASE="$AGEND_TEST_PASSPHRASE"

check_passphrase() {
    local name=$1 submit_key=$2 inject_prefix=$3 typed=$4
    local result=$(python3 - "$name" "$PASSPHRASE" "$submit_key" "$inject_prefix" "$typed" << 'PYEOF'
import socket, struct, os, glob, time, sys
name, passphrase = sys.argv[1], sys.argv[2]
submit = sys.argv[3].encode().decode('unicode_escape').encode()
prefix = sys.argv[4].encode().decode('unicode_escape').encode() if sys.argv[4] else b""
typed = sys.argv[5] == "true"
socks = glob.glob(os.path.expanduser(f'~/.agend/run/*/agents/{name}/tui.sock'))
if not socks: print("no_socket"); exit()
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(60)
tag = s.recv(1); hdr = s.recv(4); length = struct.unpack('>I', hdr)[0]
while length > 0:
    chunk = s.recv(min(8192, length)); length -= len(chunk)
# Inject: prefix + text (typed or atomic) + 20ms delay + submit
q = b"What is the name of my pet cat?."
if typed:
    for b in (prefix + q):
        s.send(b'\x00' + struct.pack('>I', 1) + bytes([b]))
        time.sleep(0.002)
else:
    if prefix:
        s.send(b'\x00' + struct.pack('>I', len(prefix)) + prefix)
    s.send(b'\x00' + struct.pack('>I', len(q)) + q)
time.sleep(0.02)
s.send(b'\x00' + struct.pack('>I', len(submit)) + submit)
# Read output stream for up to 45s
all_out = b""
deadline = time.time() + 45
while time.time() < deadline:
    try:
        tag = s.recv(1)
        if not tag: break
        hdr = s.recv(4)
        length = struct.unpack('>I', hdr)[0]
        chunk = b''
        while len(chunk) < length: chunk += s.recv(length - len(chunk))
        all_out += chunk
        import re as _re
        clean = _re.sub(r'\x1b\[[0-9;?]*[A-Za-z]', '', all_out.decode('utf-8', errors='replace'))
        if passphrase in clean:
            print("ok"); s.close(); exit()
    except socket.timeout:
        break
print("fail")
s.close()
PYEOF
)
    if [ "$result" = "ok" ]; then pass "$name: instructions effective (passphrase found)"; else fail "$name: instructions NOT effective"; fi
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

    # Test 2: Normal startup + MCP + reconnect + resize + instructions
    rm -rf ~/.agend/run/
    local workdir="/tmp/agend-claude-normal-$$"
    mkdir -p "$workdir"
    cd "$workdir"
    "$ORIG_DIR/target/debug/agend-daemon" "claude-test:claude --dangerously-skip-permissions" \
        2>/tmp/agend-claude-test.log &
    DAEMON_PID=$!
    cd "$ORIG_DIR"

    echo "  Testing ready + MCP + reconnect + resize + instructions..."
    if wait_for_pattern "claude-test" "❯\|opus\|sonnet" 30; then
        pass "Claude: ready"
    else
        fail "Claude: not ready"; cleanup_daemon; rm -rf "$workdir"; return
    fi

    check_mcp "claude-test"
    check_reconnect "claude-test"
    check_resize "claude-test"
    check_instructions "claude-test" "claude" "$workdir"
    check_passphrase "claude-test" "\r" "" "false"
    cleanup_daemon
    rm -rf "$workdir"
    pass "Claude: shutdown clean"
}

test_gemini() {
    echo ""
    echo "=== Gemini CLI ==="
    rm -rf ~/.agend/run/

    local workdir="/tmp/agend-gemini-test-$$"
    mkdir -p "$workdir"
    ORIG_DIR=$(pwd)
    cd "$workdir"
    "$ORIG_DIR/target/debug/agend-daemon" "gemini-test:gemini --yolo" \
        2>/tmp/agend-gemini-test.log &
    DAEMON_PID=$!
    cd "$ORIG_DIR"

    echo "  Testing ready + MCP + reconnect + resize + instructions..."
    if wait_for_pattern "gemini-test" ">\|gemini\|Gemini" 30; then
        pass "Gemini: ready"
    else
        fail "Gemini: not ready"; cleanup_daemon; rm -rf "$workdir"; return
    fi

    check_mcp "gemini-test"
    check_reconnect "gemini-test"
    check_resize "gemini-test"
    check_instructions "gemini-test" "gemini" "$workdir"
    check_passphrase "gemini-test" "\n\r" "\r" "true"
    cleanup_daemon
    rm -rf "$workdir"
    pass "Gemini: shutdown clean"
}

test_codex() {
    echo ""
    echo "=== Codex ==="
    rm -rf ~/.agend/run/

    local workdir="/tmp/agend-codex-test-$$"
    mkdir -p "$workdir"
    ORIG_DIR=$(pwd)
    cd "$workdir"
    "$ORIG_DIR/target/debug/agend-daemon" "codex-test:codex --full-auto" \
        2>/tmp/agend-codex-test.log &
    DAEMON_PID=$!
    cd "$ORIG_DIR"

    echo "  Testing ready + MCP + reconnect + resize + instructions..."
    if wait_for_pattern "codex-test" ">\|codex\|sandbox\|Codex" 30; then
        pass "Codex: ready"
    else
        fail "Codex: not ready"; cleanup_daemon; rm -rf "$workdir"; return
    fi

    check_mcp "codex-test"
    check_reconnect "codex-test"
    check_resize "codex-test"
    check_instructions "codex-test" "codex" "$workdir"
    check_passphrase "codex-test" "\r" "" "false"
    cleanup_daemon
    rm -rf "$workdir"
    pass "Codex: shutdown clean"
}

test_kiro() {
    echo ""
    echo "=== Kiro CLI ==="
    rm -rf ~/.agend/run/

    local workdir="/tmp/agend-kiro-test-$$"
    mkdir -p "$workdir"
    ORIG_DIR=$(pwd)
    cd "$workdir"
    "$ORIG_DIR/target/debug/agend-daemon" "kiro-test:kiro-cli chat --trust-all-tools" \
        2>/tmp/agend-kiro-test.log &
    DAEMON_PID=$!
    cd "$ORIG_DIR"

    echo "  Testing ready + MCP + reconnect + resize + instructions..."
    if wait_for_pattern "kiro-test" ">\|kiro\|trusted\|tools" 30; then
        pass "Kiro: ready"
    else
        fail "Kiro: not ready"; cleanup_daemon; rm -rf "$workdir"; return
    fi

    check_mcp "kiro-test"
    check_reconnect "kiro-test"
    check_resize "kiro-test"
    check_instructions "kiro-test" "kiro" "$workdir"
    check_passphrase "kiro-test" "\r" "" "false"
    cleanup_daemon
    rm -rf "$workdir"
    pass "Kiro: shutdown clean"
}

test_opencode() {
    echo ""
    echo "=== OpenCode ==="
    rm -rf ~/.agend/run/

    local workdir="/tmp/agend-oc-test-$$"
    mkdir -p "$workdir"
    ORIG_DIR=$(pwd)
    cd "$workdir"
    "$ORIG_DIR/target/debug/agend-daemon" "oc-test:opencode" \
        2>/tmp/agend-oc-test.log &
    DAEMON_PID=$!
    cd "$ORIG_DIR"

    echo "  Testing ready + MCP + reconnect + resize + instructions..."
    if wait_for_pattern "oc-test" ">\|opencode\|OpenCode" 30; then
        pass "OpenCode: ready"
    else
        fail "OpenCode: not ready"; cleanup_daemon; rm -rf "$workdir"; return
    fi

    check_mcp "oc-test"
    check_reconnect "oc-test"
    check_resize "oc-test"
    check_instructions "oc-test" "opencode" "$workdir"
    check_passphrase "oc-test" "\r" "\r" "false"
    cleanup_daemon
    rm -rf "$workdir"
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
