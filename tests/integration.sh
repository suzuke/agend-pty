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

if ls ~/.agend/run/*/ctrl.port >/dev/null 2>&1; then pass "daemon started"; else fail "daemon not started"; fi
if ls ~/.agend/run/*/agents/alice/tui.port >/dev/null 2>&1; then pass "alice port file"; else fail "alice port file"; fi
if ls ~/.agend/run/*/agents/bob/tui.port >/dev/null 2>&1; then pass "bob port file"; else fail "bob port file"; fi
if ls ~/.agend/run/*/api.port >/dev/null 2>&1; then pass "api port file"; else fail "api port file"; fi

echo ""
echo "=== Test 2: TUI connect + VTerm dump ==="
RESULT=$(python3 -c "
import socket, struct, os, glob
ports = glob.glob(os.path.expanduser('~/.agend/run/*/agents/alice/tui.port'))
port = int(open(ports[0]).read().strip())
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.connect(('127.0.0.1', port))
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
ports = glob.glob(os.path.expanduser('~/.agend/run/*/agents/alice/tui.port'))
port = int(open(ports[0]).read().strip())
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.connect(('127.0.0.1', port))
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
echo "=== Test 4: MCP handshake + tools (via agend-mcp bridge) ==="
RESULT=$(python3 -c "
import subprocess, json, select, os
proc = subprocess.Popen(
    ['./target/debug/agend-mcp'],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    env={**os.environ, 'AGEND_INSTANCE_NAME': 'alice'}
)
def call(req):
    proc.stdin.write((json.dumps(req) + '\n').encode())
    proc.stdin.flush()
    if not select.select([proc.stdout], [], [], 5)[0]: return None
    return json.loads(proc.stdout.readline().decode().strip())
r = call({'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2024-11-05','capabilities':{},'clientInfo':{'name':'test'}}})
assert r['result']['serverInfo']['name'] == 'agend'
r = call({'jsonrpc':'2.0','id':2,'method':'tools/list'})
tools = [t['name'] for t in r['result']['tools']]
assert 'send_to_instance' in tools
assert 'inbox' in tools
print('ok:' + ','.join(tools))
proc.terminate()
" 2>&1)
if echo "$RESULT" | grep -q "ok:"; then pass "MCP handshake + tools ($RESULT)"; else fail "MCP: $RESULT"; fi

echo ""
echo "=== Test 5: Inter-agent messaging ==="
RESULT=$(python3 -c "
import subprocess, json, select, os, socket, struct, glob, time
# Send from alice to bob via agend-mcp
proc = subprocess.Popen(
    ['./target/debug/agend-mcp'],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    env={**os.environ, 'AGEND_INSTANCE_NAME': 'alice'}
)
def call(req):
    proc.stdin.write((json.dumps(req) + '\n').encode())
    proc.stdin.flush()
    if not select.select([proc.stdout], [], [], 5)[0]: return None
    return json.loads(proc.stdout.readline().decode().strip())
call({'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2024-11-05','capabilities':{},'clientInfo':{'name':'test'}}})
call({'jsonrpc':'2.0','id':2,'method':'tools/call','params':{'name':'send_to_instance','arguments':{'instance_name':'bob','message':'INTER_AGENT_MSG'}}})
proc.terminate()
# Check bob's scrollback
time.sleep(0.5)
ports = glob.glob(os.path.expanduser('~/.agend/run/*/agents/bob/tui.port'))
port = int(open(ports[0]).read().strip())
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.connect(('127.0.0.1', port))
s.settimeout(3)
s.recv(1); hdr = s.recv(4); length = struct.unpack('>I', hdr)[0]
data = s.recv(length).decode('utf-8', errors='replace')
print('ok' if 'INTER_AGENT_MSG' in data else 'fail')
s.close()
" 2>&1)
if [ "$RESULT" = "ok" ]; then pass "inter-agent messaging"; else fail "inter-agent: $RESULT"; fi

echo ""
echo "=== Test 6: API port ==="
RESULT=$(python3 -c "
import socket, json, os, glob
ports = glob.glob(os.path.expanduser('~/.agend/run/*/api.port'))
port = int(open(ports[0]).read().strip())
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.connect(('127.0.0.1', port))
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
if [ "$RESULT" = "ok" ]; then pass "API port"; else fail "API: $RESULT"; fi

echo ""
echo "=== Test 7: Inbox (long message) ==="
RESULT=$(python3 -c "
import subprocess, json, select, os
# Send long message alice→bob via agend-mcp
proc = subprocess.Popen(
    ['./target/debug/agend-mcp'],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    env={**os.environ, 'AGEND_INSTANCE_NAME': 'alice'}
)
def call(req):
    proc.stdin.write((json.dumps(req) + '\n').encode())
    proc.stdin.flush()
    if not select.select([proc.stdout], [], [], 5)[0]: return None
    return json.loads(proc.stdout.readline().decode().strip())
call({'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2024-11-05','capabilities':{},'clientInfo':{'name':'test'}}})
call({'jsonrpc':'2.0','id':2,'method':'tools/call','params':{'name':'send_to_instance','arguments':{'instance_name':'bob','message':'X'*600}}})
proc.terminate()
# Read bob's inbox
proc = subprocess.Popen(
    ['./target/debug/agend-mcp'],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    env={**os.environ, 'AGEND_INSTANCE_NAME': 'bob'}
)
def call2(req):
    proc.stdin.write((json.dumps(req) + '\n').encode())
    proc.stdin.flush()
    if not select.select([proc.stdout], [], [], 5)[0]: return None
    return json.loads(proc.stdout.readline().decode().strip())
call2({'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2024-11-05','capabilities':{},'clientInfo':{'name':'test'}}})
r = call2({'jsonrpc':'2.0','id':2,'method':'tools/call','params':{'name':'inbox','arguments':{'id':1}}})
proc.terminate()
text = r['result']['content'][0]['text']
assert len(text) > 500
print('ok')
" 2>&1)
if [ "$RESULT" = "ok" ]; then pass "inbox long message"; else fail "inbox: $RESULT"; fi

echo ""
echo "=== Test 8: Session reaper ==="
# Send exit to alice
python3 -c "
import socket, struct, os, glob
ports = glob.glob(os.path.expanduser('~/.agend/run/*/agents/alice/tui.port'))
port = int(open(ports[0]).read().strip())
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.connect(('127.0.0.1', port))
s.settimeout(3)
tag = s.recv(1); hdr = s.recv(4); length = struct.unpack('>I', hdr)[0]; s.recv(length)
cmd = b'exit\r'
s.send(b'\x00' + struct.pack('>I', len(cmd)) + cmd)
s.close()
" 2>/dev/null
sleep 2
if ! ls ~/.agend/run/*/agents/alice/tui.port >/dev/null 2>&1; then pass "session reaped (alice removed)"; else fail "session not reaped"; fi
if ls ~/.agend/run/*/agents/bob/tui.port >/dev/null 2>&1; then pass "bob still alive"; else fail "bob gone"; fi

echo ""
echo "=== Test 9: MCP server (stdio↔TCP) ==="
RESULT=$(python3 -c "
import subprocess, json, time, os, glob, select
# Find the API port
ports = glob.glob(os.path.expanduser('~/.agend/run/*/api.port'))
if not ports:
    print('fail:no_port')
    exit()
port = open(ports[0]).read().strip()
proc = subprocess.Popen(
    ['./target/debug/agend-mcp', '--port', port],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    env={**os.environ, 'AGEND_INSTANCE_NAME': 'bob'}
)
# MCP server expects NDJSON on stdin, returns NDJSON on stdout
req = json.dumps({'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2024-11-05','capabilities':{},'clientInfo':{'name':'test'}}})
proc.stdin.write((req + '\n').encode())
proc.stdin.flush()
# Read NDJSON response line
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
if ! ls ~/.agend/run/*/ctrl.port >/dev/null 2>&1; then pass "shutdown + cleanup"; else fail "cleanup incomplete"; fi

echo ""
echo "════════════════════════════════"
echo "Results: $PASS passed, $FAIL failed"
if [ $FAIL -gt 0 ]; then
    echo "FAILED"
    exit 1
else
    echo "ALL PASSED ✅"
fi
