#!/bin/bash
# Corelamo auth flow — automated test script
#
# HOW TO USE:
#   1. Start the server first:  cargo run -p core-runtime -- --root-path /tmp
#   2. Run this whole file:     bash auth_test.sh
#
# Each test prints PASS or FAIL based on the actual HTTP status code
# returned, compared against the expected one. No manual reading needed.

BASE_URL="http://localhost:6006"
PASS_COUNT=0
FAIL_COUNT=0

# ── helper: run a curl command, check status code, print PASS/FAIL ────
# usage: check "<label>" <expected_status> <curl args...>
check() {
    local label="$1"
    local expected="$2"
    shift 2
    local response
    response=$(curl -s -o /tmp/auth_test_body.json -w "%{http_code}" "$@")

    if [ "$response" == "$expected" ]; then
        echo "PASS  [$label] got $response"
        PASS_COUNT=$((PASS_COUNT + 1))
    else
        echo "FAIL  [$label] expected $expected, got $response"
        echo "      body: $(cat /tmp/auth_test_body.json)"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
}

# ── helper: run a curl command, extract a token via jq ─────────────────
get_token() {
    curl -s -X POST "$BASE_URL/api/login" -d "$1" | jq -r '.data.token'
}

echo "=========================================="
echo " Corelamo auth test suite"
echo "=========================================="

# ── 1. login as admin ──────────────────────────────────────────────────
check "login as admin" 200 \
  -X POST "$BASE_URL/api/login" -d '{"username":"admin","password":"secret"}'

ADMIN_TOKEN=$(get_token '{"username":"admin","password":"secret"}')

# ── 2. login with wrong password ────────────────────────────────────────
check "login with wrong password" 401 \
  -X POST "$BASE_URL/api/login" -d '{"username":"admin","password":"wrongpassword"}'

# ── 3. search with valid admin token ────────────────────────────────────
check "admin search (valid token)" 200 \
  -X POST "$BASE_URL/api/databases/movies/search" \
  -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"query":"batman", "docs":3}'

# ── 4. search with no token ─────────────────────────────────────────────
check "search with no token" 401 \
  -X POST "$BASE_URL/api/databases/movies/search" \
  -d '{"query":"batman", "docs":3}'

# ── 5. search with fake token ───────────────────────────────────────────
check "search with fake token" 401 \
  -X POST "$BASE_URL/api/databases/movies/search" \
  -H "X-Corelamo-Key: not-a-real-token" -d '{"query":"batman", "docs":3}'

# ── 6. viewer / editor role boundary tests ──────────────────────────────
# create viewer and editor first — bootstrap() only seeds "admin"
curl -s -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"username":"viewer","password":"secret","roles":["viewer"]}' > /dev/null
curl -s -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"username":"editor","password":"secret","roles":["editor"]}' > /dev/null

VIEWER_TOKEN=$(get_token '{"username":"viewer","password":"secret"}')
EDITOR_TOKEN=$(get_token '{"username":"editor","password":"secret"}')

check "viewer can search" 200 \
  -X POST "$BASE_URL/api/databases/movies/search" \
  -H "X-Corelamo-Key: $VIEWER_TOKEN" -d '{"query":"batman", "docs":3}'

check "viewer cannot insert" 403 \
  -X POST "$BASE_URL/api/databases/movies/insert" \
  -H "X-Corelamo-Key: $VIEWER_TOKEN" -d '{"id":"test-doc-1","title":"Test"}'

check "editor can insert" 200 \
  -X POST "$BASE_URL/api/databases/movies/insert" \
  -H "X-Corelamo-Key: $EDITOR_TOKEN" -d '{"id":"test-doc-1","title":"Test"}'

# ── 7. user CRUD lifecycle (admin only) ─────────────────────────────────
check "admin creates testuser" 200 \
  -X POST "$BASE_URL/api/users" \
  -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"username":"testuser","password":"testpass123","roles":["viewer"]}'

check "testuser can log in" 200 \
  -X POST "$BASE_URL/api/login" -d '{"username":"testuser","password":"testpass123"}'

check "viewer cannot create users" 403 \
  -X POST "$BASE_URL/api/users" \
  -H "X-Corelamo-Key: $VIEWER_TOKEN" \
  -d '{"username":"shouldnotexist","password":"whatever","roles":["viewer"]}'

check "admin changes testuser password" 200 \
  -X POST "$BASE_URL/api/users/testuser/password" \
  -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"password":"newpass456"}'

check "testuser old password now fails" 401 \
  -X POST "$BASE_URL/api/login" -d '{"username":"testuser","password":"testpass123"}'

check "testuser new password works" 200 \
  -X POST "$BASE_URL/api/login" -d '{"username":"testuser","password":"newpass456"}'

check "admin changes testuser roles to editor" 200 \
  -X POST "$BASE_URL/api/users/testuser/roles" \
  -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"roles":["editor"]}'

TESTUSER_TOKEN=$(get_token '{"username":"testuser","password":"newpass456"}')

check "testuser can now insert (has editor role)" 200 \
  -X POST "$BASE_URL/api/databases/movies/insert" \
  -H "X-Corelamo-Key: $TESTUSER_TOKEN" -d '{"id":"test-doc-1","title":"Test"}'

check "admin deletes testuser" 200 \
  -X DELETE "$BASE_URL/api/users/testuser" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "deleted testuser can no longer log in" 401 \
  -X POST "$BASE_URL/api/login" -d '{"username":"testuser","password":"newpass456"}'

check "deleting already-deleted testuser fails" 404 \
  -X DELETE "$BASE_URL/api/users/testuser" -H "X-Corelamo-Key: $ADMIN_TOKEN"

echo "=========================================="
echo " Results: $PASS_COUNT passed, $FAIL_COUNT failed"
echo "=========================================="

rm -f /tmp/auth_test_body.json

if [ "$FAIL_COUNT" -gt 0 ]; then
    exit 1
fi