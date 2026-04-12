#!/bin/bash
# Integration test for agend-pty
# Runs the full daemon lifecycle with bash agents (no Claude needed)
set -e

cd "$(dirname "$0")/.."
PASS=0
FAIL=0

pass() { echo "  ✅ $1"; PASS=$((PASS+1)); }
fail() { echo "  ❌ $1"; FAIL=$((FAIL+1)); }

echo "Building..."
cargo build --quiet 2>/dev/null

# Clean up any previous state
pkill -f "target/debug/agend-daemon" 2>/dev/null || true
sleep 1
rm -rf ~/.agend/run/

echo ""
echo "=== Test 1: Daemon startup from CLI args ==="
cargo run --quiet --bin agend-daemon -- alice:bash bob:bash 2>/tmp/agend-test.log &
DAEMON_PID=$!
sleep 2

if ls ~/.agend/run/*/ctrl.sock >/dev/null 2>&1; then pass "daemon started"; else fail "daemon not started"; fi
if ls ~/.agend/run/*/agents/alice/tui.sock >/dev/null 2>&1; then pass "alice socket"; else fail "alice socket"; fi
if ls ~/.agend/run/*/agents/bob/tui.sock >/dev/null 2>&1; then pass "bob socket"; else fail "bob socket"; fi
if ls ~/.agend/run/*/api.sock >/dev/null 2>&1; then pass "api socket"; else fail "api socket"; fi

echo ""
echo "=== Test 2: TUI connect + VTerm dump ==="
RESULT=$(python3 -c "
import socket, struct, os, glob
socks = glob.glob(os.path.expanduser('~/.agend/run/*/agents/alice/tui.sock'))
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(3)
tag = s.recv(1)
hdr = s.recv(4)
length = struct.unpack('>I', hdr)[0]
print(f'ok:{length}')
s.close()
" 2>&1)
if echo "$RESULT" | grep -q "ok:"; then pass "TUI connect + screen dump ($RESULT)"; else fail "TUI connect: $RESULT"; fi

echo ""
echo "=== Test 3: TUI send command + receive output ==="
RESULT=$(python3 -c "
import socket, struct, os, glob, time
socks = glob.glob(os.path.expanduser('~/.agend/run/*/agents/alice/tui.sock'))
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(3)
# Read screen dump
s.recv(1); hdr = s.recv(4); length = struct.unpack('>I', hdr)[0]
s.recv(length)
# Send command
cmd = b'echo INTEGRATION_TEST_42\r'
s.send(b'\x00' + struct.pack('>I', len(cmd)) + cmd)
time.sleep(0.5)
# Read output
found = False
try:
    for _ in range(20):
        s.recv(1); hdr = s.recv(4); length = struct.unpack('>I', hdr)[0]
        data = s.recv(length)
        if b'INTEGRATION_TEST_42' in data: found = True; break
except: pass
print('ok' if found else 'fail')
s.close()
" 2>&1)
if [ "$RESULT" = "ok" ]; then pass "command round-trip"; else fail "command round-trip: $RESULT"; fi

echo ""
echo "=== Test 4: MCP handshake + tools ==="
RESULT=$(python3 -c "
import socket, json, os, glob
socks = glob.glob(os.path.expanduser('~/.agend/run/*/agents/alice/mcp.sock'))
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(5)
def call(method, params=None, id=1):
    req = {'jsonrpc': '2.0', 'id': id, 'method': method}
    if params: req['params'] = params
    body = json.dumps(req)
    s.send(f'Content-Length: {len(body)}\r\n\r\n{body}'.encode())
    h = b''
    while b'\r\n\r\n' not in h: h += s.recv(1)
    cl = int([l for l in h.decode().split('\r\n') if 'Content-Length' in l][0].split(':')[1].strip())
    return json.loads(s.recv(cl))
r = call('initialize', {'protocolVersion': '2024-11-05', 'capabilities': {}, 'clientInfo': {'name': 'test'}})
assert r['result']['serverInfo']['name'] == 'agend'
r = call('tools/list', id=2)
tools = [t['name'] for t in r['result']['tools']]
assert 'send_to_instance' in tools
assert 'inbox' in tools
print('ok:' + ','.join(tools))
s.close()
" 2>&1)
if echo "$RESULT" | grep -q "ok:"; then pass "MCP handshake + tools ($RESULT)"; else fail "MCP: $RESULT"; fi

echo ""
echo "=== Test 5: Inter-agent messaging ==="
RESULT=$(python3 -c "
import socket, json, os, glob, struct, time
# Send from alice to bob via MCP
socks = glob.glob(os.path.expanduser('~/.agend/run/*/agents/alice/mcp.sock'))
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(5)
def call(method, params=None, id=1):
    req = {'jsonrpc': '2.0', 'id': id, 'method': method}
    if params: req['params'] = params
    body = json.dumps(req)
    s.send(f'Content-Length: {len(body)}\r\n\r\n{body}'.encode())
    h = b''
    while b'\r\n\r\n' not in h: h += s.recv(1)
    cl = int([l for l in h.decode().split('\r\n') if 'Content-Length' in l][0].split(':')[1].strip())
    return json.loads(s.recv(cl))
call('initialize', {'protocolVersion': '2024-11-05', 'capabilities': {}, 'clientInfo': {'name': 'test'}})
r = call('tools/call', {'name': 'send_to_instance', 'arguments': {'instance_name': 'bob', 'message': 'INTER_AGENT_MSG'}}, id=2)
s.close()
# Check bob's scrollback
time.sleep(0.5)
socks = glob.glob(os.path.expanduser('~/.agend/run/*/agents/bob/tui.sock'))
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(3)
s.recv(1); hdr = s.recv(4); length = struct.unpack('>I', hdr)[0]
data = s.recv(length).decode('utf-8', errors='replace')
print('ok' if 'INTER_AGENT_MSG' in data else 'fail')
s.close()
" 2>&1)
if [ "$RESULT" = "ok" ]; then pass "inter-agent messaging"; else fail "inter-agent: $RESULT"; fi

echo ""
echo "=== Test 6: API socket ==="
RESULT=$(python3 -c "
import socket, json, os, glob
socks = glob.glob(os.path.expanduser('~/.agend/run/*/api.sock'))
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(5)
s.send(json.dumps({'method': 'list'}).encode() + b'\n')
r = json.loads(s.recv(4096))
assert r['ok']
assert 'alice' in r['result']['instances']
s.send(json.dumps({'method': 'status'}).encode() + b'\n')
r = json.loads(s.recv(4096))
assert r['ok']
print('ok')
s.close()
" 2>&1)
if [ "$RESULT" = "ok" ]; then pass "API socket"; else fail "API: $RESULT"; fi

echo ""
echo "=== Test 7: Inbox (long message) ==="
RESULT=$(python3 -c "
import socket, json, os, glob
# Send long message alice→bob
socks = glob.glob(os.path.expanduser('~/.agend/run/*/agents/alice/mcp.sock'))
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(5)
def call(method, params=None, id=1):
    req = {'jsonrpc': '2.0', 'id': id, 'method': method}
    if params: req['params'] = params
    body = json.dumps(req)
    s.send(f'Content-Length: {len(body)}\r\n\r\n{body}'.encode())
    h = b''
    while b'\r\n\r\n' not in h: h += s.recv(1)
    cl = int([l for l in h.decode().split('\r\n') if 'Content-Length' in l][0].split(':')[1].strip())
    return json.loads(s.recv(cl))
call('initialize', {'protocolVersion': '2024-11-05', 'capabilities': {}, 'clientInfo': {'name': 'test'}})
call('tools/call', {'name': 'send_to_instance', 'arguments': {'instance_name': 'bob', 'message': 'X' * 600}}, id=2)
s.close()
# Read bob's inbox
socks = glob.glob(os.path.expanduser('~/.agend/run/*/agents/bob/mcp.sock'))
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(5)
call('initialize', {'protocolVersion': '2024-11-05', 'capabilities': {}, 'clientInfo': {'name': 'test'}})
r = call('tools/call', {'name': 'inbox', 'arguments': {'id': 1}}, id=2)
text = r['result']['content'][0]['text']
assert len(text) > 500
print('ok')
s.close()
" 2>&1)
if [ "$RESULT" = "ok" ]; then pass "inbox long message"; else fail "inbox: $RESULT"; fi

echo ""
echo "=== Test 8: Session reaper ==="
RESULT=$(python3 -c "
import socket, struct, os, glob, time
socks = glob.glob(os.path.expanduser('~/.agend/run/*/agents/alice/tui.sock'))
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(3)
s.recv(1); s.recv(4); s.recv(struct.unpack('>I', s.recv(4))[0])  # drain
# wrong: need to read tag first for drain. let me just drain the screen dump properly
" 2>&1 || true)
# Send exit to alice
python3 -c "
import socket, struct, os, glob
socks = glob.glob(os.path.expanduser('~/.agend/run/*/agents/alice/tui.sock'))
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(socks[0])
s.settimeout(3)
tag = s.recv(1); hdr = s.recv(4); length = struct.unpack('>I', hdr)[0]; s.recv(length)
cmd = b'exit\r'
s.send(b'\x00' + struct.pack('>I', len(cmd)) + cmd)
s.close()
" 2>/dev/null
sleep 2
if ! ls ~/.agend/run/*/agents/alice/tui.sock >/dev/null 2>&1; then pass "session reaped (alice removed)"; else fail "session not reaped"; fi
if ls ~/.agend/run/*/agents/bob/tui.sock >/dev/null 2>&1; then pass "bob still alive"; else fail "bob gone"; fi

echo ""
echo "=== Test 9: MCP server (stdio↔socket) ==="
RESULT=$(python3 -c "
import subprocess, json, time, os, glob
# Find the API socket
socks = glob.glob(os.path.expanduser('~/.agend/run/*/api.sock'))
if not socks:
    print('fail:no_socket')
    exit()
proc = subprocess.Popen(
    ['./target/debug/agend-mcp', '--socket', socks[0]],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    env={**os.environ, 'AGEND_INSTANCE_NAME': 'bob'}
)
# MCP server expects NDJSON on stdin, returns NDJSON on stdout
req = json.dumps({'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2024-11-05','capabilities':{},'clientInfo':{'name':'test'}}})
proc.stdin.write((req + '\n').encode())
proc.stdin.flush()
# Read NDJSON response line
import select
if select.select([proc.stdout], [], [], 5)[0]:
    line = proc.stdout.readline().decode().strip()
    if line:
        r = json.loads(line)
        assert r['result']['serverInfo']['name'] == 'agend'
        print('ok')
    else:
        print('fail:empty')
else:
    print('fail:timeout')
proc.terminate()
" 2>&1)
if [ "$RESULT" = "ok" ]; then pass "MCP bridge"; else fail "MCP bridge: $RESULT"; fi

echo ""
echo "=== Test 10: Graceful shutdown ==="
cargo run --quiet --bin agend-daemon -- --shutdown 2>/dev/null
sleep 2
if ! ls ~/.agend/run/*/ctrl.sock >/dev/null 2>&1; then pass "shutdown + cleanup"; else fail "cleanup incomplete"; fi

echo ""
echo "════════════════════════════════"
echo "Results: $PASS passed, $FAIL failed"
if [ $FAIL -gt 0 ]; then
    echo "FAILED"
    exit 1
else
    echo "ALL PASSED ✅"
fi
