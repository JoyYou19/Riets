#!/bin/bash
# Corelamo auth flow — full automated test suite
#
# HOW TO USE:
#   1. Start the server first:  cargo run -p core-runtime -- --root-path /tmp
#   2. Run this whole file:     bash auth_test.sh
#
# IMPORTANT: any test below that unexpectedly returns 200 instead of a
# 403 means that handler is still missing its Permission::X check —
# same bug class as insert_handler had. See the bottom of this file
# for the fix pattern and which Permission goes with which handler.

BASE_URL="http://localhost:6006"
PASS_COUNT=0
FAIL_COUNT=0
RUN_ID=$(date +%s)
TESTDB="testdb-$RUN_ID"

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

get_token() {
    curl -s -X POST "$BASE_URL/api/login" -d "$1" | jq -r '.data.token'
}

echo "=========================================="
echo " Corelamo auth test suite"
echo "=========================================="

# ── SECTION 1: LOGIN BASICS ─────────────────────────────────────────────
check "login as admin" 200 \
  -X POST "$BASE_URL/api/login" -d '{"username":"admin","password":"secret"}'

ADMIN_TOKEN=$(get_token '{"username":"admin","password":"secret"}')

check "login with wrong password" 401 \
  -X POST "$BASE_URL/api/login" -d '{"username":"admin","password":"wrongpassword"}'

check "login with nonexistent user" 401 \
  -X POST "$BASE_URL/api/login" -d '{"username":"doesnotexist","password":"whatever"}'

# ── SECTION 2: TOKEN VALIDATION ─────────────────────────────────────────
check "search with no token" 401 \
  -X POST "$BASE_URL/api/databases/movies/search" -d '{"query":"batman", "docs":3}'

check "search with fake token" 401 \
  -X POST "$BASE_URL/api/databases/movies/search" \
  -H "X-Corelamo-Key: not-a-real-token" -d '{"query":"batman", "docs":3}'

check "admin search (valid token)" 200 \
  -X POST "$BASE_URL/api/databases/movies/search" \
  -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"query":"batman", "docs":3}'

# ── SECTION 3: CREATE TEST USERS FOR EACH ROLE ──────────────────────────
curl -s -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"username":"viewer","password":"secret","roles":["viewer"]}' > /dev/null
curl -s -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"username":"editor","password":"secret","roles":["editor"]}' > /dev/null
curl -s -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"username":"architect","password":"secret","roles":["architect"]}' > /dev/null

VIEWER_TOKEN=$(get_token '{"username":"viewer","password":"secret"}')
EDITOR_TOKEN=$(get_token '{"username":"editor","password":"secret"}')
ARCHITECT_TOKEN=$(get_token '{"username":"architect","password":"secret"}')

# ── SECTION 4: SEARCH permission (Permission::Search) ───────────────────
check "viewer can search (has Search)" 200 \
  -X POST "$BASE_URL/api/databases/movies/search" \
  -H "X-Corelamo-Key: $VIEWER_TOKEN" -d '{"query":"batman", "docs":3}'

check "architect cannot search (no Search)" 403 \
  -X POST "$BASE_URL/api/databases/movies/search" \
  -H "X-Corelamo-Key: $ARCHITECT_TOKEN" -d '{"query":"batman", "docs":3}'

# ── SECTION 5: INSERT permission (Permission::Insert) ───────────────────
check "viewer cannot insert (no Insert)" 403 \
  -X POST "$BASE_URL/api/databases/movies/insert" \
  -H "X-Corelamo-Key: $VIEWER_TOKEN" -d '{"id":"test-doc-viewer-'"$RUN_ID"'","title":"Test"}'

check "editor can insert (has Insert)" 200 \
  -X POST "$BASE_URL/api/databases/movies/insert" \
  -H "X-Corelamo-Key: $EDITOR_TOKEN" -d '{"id":"test-doc-editor-'"$RUN_ID"'","title":"Test"}'

# ── SECTION 6: RETRIEVE permission (Permission::Retrieve) ───────────────
check "viewer can retrieve (has Retrieve)" 200 \
  -X POST "$BASE_URL/api/databases/movies/retrieve" \
  -H "X-Corelamo-Key: $VIEWER_TOKEN" -d '["test-doc-editor-'"$RUN_ID"'"]'

check "architect cannot retrieve (no Retrieve)" 403 \
  -X POST "$BASE_URL/api/databases/movies/retrieve" \
  -H "X-Corelamo-Key: $ARCHITECT_TOKEN" -d '["test-doc-editor-'"$RUN_ID"'"]'

# ── SECTION 7: DATABASE MANAGEMENT — uses throwaway $TESTDB, not movies ─
check "architect can create-database (has CreateDatabase)" 200 \
  -X POST "$BASE_URL/api/databases/$TESTDB/create-database" \
  -H "X-Corelamo-Key: $ARCHITECT_TOKEN"

check "viewer cannot create-database (no CreateDatabase)" 403 \
  -X POST "$BASE_URL/api/databases/viewer-should-not-exist-$RUN_ID/create-database" \
  -H "X-Corelamo-Key: $VIEWER_TOKEN"

check "architect can list databases (has ListDatabase)" 200 \
  -X GET "$BASE_URL/api/databases" -H "X-Corelamo-Key: $ARCHITECT_TOKEN"

check "viewer cannot list databases (no ListDatabase)" 403 \
  -X GET "$BASE_URL/api/databases" -H "X-Corelamo-Key: $VIEWER_TOKEN"

check "architect can check status (has Status)" 200 \
  -X GET "$BASE_URL/api/databases/$TESTDB/status" -H "X-Corelamo-Key: $ARCHITECT_TOKEN"

check "viewer cannot check status (no Status)" 403 \
  -X GET "$BASE_URL/api/databases/$TESTDB/status" -H "X-Corelamo-Key: $VIEWER_TOKEN"

check "architect can get policy (has GetPolicy)" 200 \
  -X GET "$BASE_URL/api/databases/$TESTDB/policy" -H "X-Corelamo-Key: $ARCHITECT_TOKEN"

check "viewer cannot get policy (no GetPolicy)" 403 \
  -X GET "$BASE_URL/api/databases/$TESTDB/policy" -H "X-Corelamo-Key: $VIEWER_TOKEN"

check "viewer cannot delete-database (no DeleteDatabase)" 403 \
  -X DELETE "$BASE_URL/api/databases/$TESTDB/delete-database" -H "X-Corelamo-Key: $VIEWER_TOKEN"

check "architect can delete-database (has DeleteDatabase)" 200 \
  -X DELETE "$BASE_URL/api/databases/$TESTDB/delete-database" -H "X-Corelamo-Key: $ARCHITECT_TOKEN"

# ── SECTION 8: USER CRUD (Permission::CreateUser / DeleteUser / UpdatePwd / UpdateRole) ─
check "admin creates testuser" 200 \
  -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"username":"testuser","password":"testpass123","roles":["viewer"]}'

check "viewer cannot create users" 403 \
  -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $VIEWER_TOKEN" \
  -d '{"username":"shouldnotexist","password":"whatever","roles":["viewer"]}'

check "testuser can log in" 200 \
  -X POST "$BASE_URL/api/login" -d '{"username":"testuser","password":"testpass123"}'

check "admin changes testuser password" 200 \
  -X POST "$BASE_URL/api/users/testuser/password" \
  -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"password":"newpass456"}'

check "viewer cannot change others' password" 403 \
  -X POST "$BASE_URL/api/users/testuser/password" \
  -H "X-Corelamo-Key: $VIEWER_TOKEN" -d '{"password":"hacked"}'

check "testuser old password now fails" 401 \
  -X POST "$BASE_URL/api/login" -d '{"username":"testuser","password":"testpass123"}'

check "testuser new password works" 200 \
  -X POST "$BASE_URL/api/login" -d '{"username":"testuser","password":"newpass456"}'

check "admin changes testuser roles to editor" 200 \
  -X POST "$BASE_URL/api/users/testuser/roles" \
  -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"roles":["editor"]}'

check "viewer cannot change others' roles" 403 \
  -X POST "$BASE_URL/api/users/testuser/roles" \
  -H "X-Corelamo-Key: $VIEWER_TOKEN" -d '{"roles":["admin"]}'

TESTUSER_TOKEN=$(get_token '{"username":"testuser","password":"newpass456"}')

check "testuser can now insert (has editor role)" 200 \
  -X POST "$BASE_URL/api/databases/movies/insert" \
  -H "X-Corelamo-Key: $TESTUSER_TOKEN" -d '{"id":"test-doc-testuser-'"$RUN_ID"'","title":"Test"}'

check "viewer cannot delete users" 403 \
  -X DELETE "$BASE_URL/api/users/testuser" -H "X-Corelamo-Key: $VIEWER_TOKEN"

check "admin deletes testuser" 200 \
  -X DELETE "$BASE_URL/api/users/testuser" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "deleted testuser can no longer log in" 401 \
  -X POST "$BASE_URL/api/login" -d '{"username":"testuser","password":"newpass456"}'

check "deleting already-deleted testuser fails" 404 \
  -X DELETE "$BASE_URL/api/users/testuser" -H "X-Corelamo-Key: $ADMIN_TOKEN"

# ── SECTION 9: ADMIN CAN DO EVERYTHING (sanity check) ───────────────────
check "admin can search" 200 \
  -X POST "$BASE_URL/api/databases/movies/search" \
  -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"query":"batman", "docs":3}'

check "admin can list databases" 200 \
  -X GET "$BASE_URL/api/databases" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "admin can check movies status" 200 \
  -X GET "$BASE_URL/api/databases/movies/status" -H "X-Corelamo-Key: $ADMIN_TOKEN"

echo "=========================================="
echo " Results: $PASS_COUNT passed, $FAIL_COUNT failed"
echo "=========================================="
echo ""
echo "If any test unexpectedly returned 200 instead of 403, that handler"
echo "is still missing its permission check. Fix pattern (same as"
echo "insert_handler): add near the top of the handler, before any"
echo "database work:"
echo ""
echo '  let Ok(auth) = state.auth.read() else {'
echo '      return HttpError::from_corelamo('
echo '          CorelamoError::Internal("auth service lock poisoned".to_string()),'
echo '          &ctx,'
echo '      ).into_response();'
echo '  };'
echo '  if let Err(e) = auth.check(&principal, Permission::X) {'
echo '      return HttpError::from_corelamo(e, &ctx).into_response();'
echo '  }'
echo '  drop(auth);'
echo ""
echo "Handler -> Permission mapping:"
echo "  retrieve_handler        -> Permission::Retrieve"
echo "  create_handler          -> Permission::CreateDatabase"
echo "  delete_handler           -> Permission::DeleteDatabase"
echo "  list_databases_handler   -> Permission::ListDatabase"
echo "  stats_handler            -> Permission::Status"
echo "  get_policy_handler       -> Permission::GetPolicy"
echo "  set_policy_handler       -> Permission::PostPolicy"
echo ""
echo "Also make sure each handler has 'Extension(principal): Extension<Principal>'"
echo "as a parameter — add it if missing."

rm -f /tmp/auth_test_body.json

if [ "$FAIL_COUNT" -gt 0 ]; then
    exit 1
fi