#!/bin/bash
# CLI POC — technical verification of agent CLI core paths.
# Tests: clap parsing, stdin pipe, heredoc, API socket, JSON output.
set -e

BIN="$(dirname "$0")/../target/debug/agend-pty"
PASS=0
FAIL=0

pass() { echo "  ✅ $1"; PASS=$((PASS+1)); }
fail() { echo "  ❌ $1: $2"; FAIL=$((FAIL+1)); }

echo "Building..."
cargo build --bin agend-pty 2>/dev/null

echo ""
echo "=== Test 1: clap parsing ==="

# Positional args
OUT=$($BIN agent send alice "hello world" 2>&1) || true
echo "$OUT" | grep -q '"error"' && pass "send positional parsed (daemon not running = expected)" || fail "send positional" "$OUT"

OUT=$($BIN agent reply "test reply" 2>&1) || true
echo "$OUT" | grep -q '"error"' && pass "reply positional parsed" || fail "reply positional" "$OUT"

OUT=$($BIN agent list 2>&1) || true
echo "$OUT" | grep -q '"error"' && pass "list parsed" || fail "list" "$OUT"

# Missing required arg → clap error
OUT=$($BIN agent send 2>&1) || true
echo "$OUT" | grep -qi "required\|usage" && pass "missing arg shows usage" || fail "missing arg" "$OUT"

echo ""
echo "=== Test 2: stdin pipe ==="

OUT=$(echo "piped message" | $BIN agent send alice --stdin 2>&1) || true
echo "$OUT" | grep -q '"error"' && pass "stdin pipe with --stdin flag" || fail "stdin pipe" "$OUT"

# Auto-detect pipe (no --stdin flag)
OUT=$(echo "auto piped" | $BIN agent send alice 2>&1) || true
echo "$OUT" | grep -q '"error"' && pass "stdin auto-detect (no --stdin)" || fail "stdin auto" "$OUT"

echo ""
echo "=== Test 3: heredoc ==="

OUT=$($BIN agent reply --stdin <<'EOF'
Code: `fn main() { println!("hello $world"); }`
"quotes" and $variables are fine.
EOF
) 2>&1 || true
echo "$OUT" | grep -q '"error"' && pass "heredoc with special chars" || fail "heredoc" "$OUT"

echo ""
echo "=== Test 4: TTY safety ==="

# No text + no pipe + TTY → should error immediately, not hang
OUT=$($BIN agent send alice 2>&1) || true
echo "$OUT" | grep -q "text argument required" && pass "TTY no-text errors cleanly" || fail "TTY safety" "$OUT"

echo ""
echo "=== Test 5: JSON output format ==="

OUT=$($BIN agent list 2>&1) || true
# Should be valid JSON
echo "$OUT" | python3 -c "import sys,json; json.load(sys.stdin)" 2>/dev/null && pass "output is valid JSON" || fail "JSON format" "$OUT"

echo ""
echo "=== Results ==="
echo "  Passed: $PASS"
echo "  Failed: $FAIL"
[ $FAIL -eq 0 ] && echo "  🎉 All tests passed!" || echo "  ⚠️  Some tests failed."
exit $FAIL
