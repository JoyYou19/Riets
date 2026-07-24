#!/bin/bash
BASE_URL="http://localhost:6006"
PASS_COUNT=0
FAIL_COUNT=0
RUN_ID=$(date +%s)

check() {
    local label="$1"; local expected="$2"; shift 2
    local response
    response=$(curl -s -o /tmp/auth_test_body.json -w "%{http_code}" "$@")
    if [ "$response" == "$expected" ]; then
        echo "PASS  [$label] got $response"; PASS_COUNT=$((PASS_COUNT + 1))
    else
        echo "FAIL  [$label] expected $expected, got $response"
        echo "      body: $(cat /tmp/auth_test_body.json)"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
}

get_token() {
    curl -s -X POST "$BASE_URL/api/login" -d "$1" | jq -r '.data.token'
}

echo "=========================================="
echo " Corelamo auth test suite"
echo "=========================================="

# ── cleanup from previous runs (ignore failures) ───────────────────────
ADMIN_TOKEN=$(get_token '{"username":"admin","password":"secret"}')
for u in reader1 writer1 testuser; do
  curl -s -X DELETE "$BASE_URL/api/users/$u" -H "X-Corelamo-Key: $ADMIN_TOKEN" > /dev/null
done

# ── 1-2. login ─────────────────────────────────────────────────────────
check "login as admin" 200 \
  -X POST "$BASE_URL/api/login" -d '{"username":"admin","password":"secret"}'

check "login with wrong password" 401 \
  -X POST "$BASE_URL/api/login" -d '{"username":"admin","password":"wrongpassword"}'

curl -s -X POST "$BASE_URL/api/databases/movies/start-database" \
  -H "X-Corelamo-Key: $ADMIN_TOKEN" 
# ── 3-5. token gate ────────────────────────────────────────────────────
check "admin search (valid token)" 200 \
  -X POST "$BASE_URL/api/databases/movies/search" \
  -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"query":"batman", "docs":3}'

check "search with no token" 401 \
  -X POST "$BASE_URL/api/databases/movies/search" -d '{"query":"batman", "docs":3}'

check "search with fake token" 401 \
  -X POST "$BASE_URL/api/databases/movies/search" \
  -H "X-Corelamo-Key: not-a-real-token" -d '{"query":"batman", "docs":3}'

# ── 6. role boundary tests ─────────────────────────────────────────────
check "admin creates reader1" 200 \
  -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"username":"reader1","password":"secret","roles":["viewer"]}'
check "admin creates writer1" 200 \
  -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"username":"writer1","password":"secret","roles":["reader"]}'

READER_TOKEN=$(get_token '{"username":"reader1","password":"secret"}')
WRITER_TOKEN=$(get_token '{"username":"writer1","password":"secret"}')

check "reader can search" 200 \
  -X POST "$BASE_URL/api/databases/movies/search" \
  -H "X-Corelamo-Key: $READER_TOKEN" -d '{"query":"batman", "docs":3}'

check "reader cannot insert" 403 \
  -X POST "$BASE_URL/api/databases/movies/insert" \
  -H "X-Corelamo-Key: $READER_TOKEN" -d '[{"id":"test-r-'"$RUN_ID"'","title":"Test"}]'

check "writer can insert" 200 \
  -X POST "$BASE_URL/api/databases/movies/insert" \
  -H "X-Corelamo-Key: $WRITER_TOKEN" -d '[{"id":"test-w-'"$RUN_ID"'","title":"Test"}]'

# ── 7. user CRUD lifecycle ─────────────────────────────────────────────
check "admin creates testuser" 200 \
  -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"username":"testuser","password":"testpass123","roles":["viewer"]}'

check "testuser can log in" 200 \
  -X POST "$BASE_URL/api/login" -d '{"username":"testuser","password":"testpass123"}'

check "reader cannot create users" 403 \
  -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $READER_TOKEN" \
  -d '{"username":"shouldnotexist","password":"whatever","roles":["viewer"]}'

check "admin changes testuser password" 200 \
  -X POST "$BASE_URL/api/users/testuser/password" \
  -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"password":"newpass456"}'

check "testuser old password now fails" 401 \
  -X POST "$BASE_URL/api/login" -d '{"username":"testuser","password":"testpass123"}'

check "testuser new password works" 200 \
  -X POST "$BASE_URL/api/login" -d '{"username":"testuser","password":"newpass456"}'

check "admin changes testuser roles to writer" 200 \
  -X POST "$BASE_URL/api/users/testuser/roles" \
  -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"roles":["editor"]}'

TESTUSER_TOKEN=$(get_token '{"username":"testuser","password":"newpass456"}')

check "testuser can now insert (writer role)" 200 \
  -X POST "$BASE_URL/api/databases/movies/insert" \
  -H "X-Corelamo-Key: $TESTUSER_TOKEN" -d '[{"id":"test-t-'"$RUN_ID"'","title":"Test"}]'

check "admin deletes testuser" 200 \
  -X DELETE "$BASE_URL/api/users/testuser" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "deleted testuser can no longer log in" 401 \
  -X POST "$BASE_URL/api/login" -d '{"username":"testuser","password":"newpass456"}'

check "deleting already-deleted testuser fails" 404 \
  -X DELETE "$BASE_URL/api/users/testuser" -H "X-Corelamo-Key: $ADMIN_TOKEN"

# ── cleanup ────────────────────────────────────────────────────────────
for u in reader1 writer1; do
  curl -s -X DELETE "$BASE_URL/api/users/$u" -H "X-Corelamo-Key: $ADMIN_TOKEN" > /dev/null
done

echo "=========================================="
echo " Results: $PASS_COUNT passed, $FAIL_COUNT failed"
echo "=========================================="
rm -f /tmp/auth_test_body.json
[ "$FAIL_COUNT" -gt 0 ] && exit 1