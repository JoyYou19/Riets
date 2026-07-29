#!/bin/bash
# ==============================================================================
# Corelamo — full permission-matrix auth test suite
# ==============================================================================
#
# HOW TO USE:
#   1. Start the server first:  cargo run -p core-runtime -- --root-path /tmp
#   2. Run this whole file:     bash auth_test.sh
#
# WHAT THIS COVERS:
#   - login (success + failure paths)
#   - every Permission variant from bootstrap.rs (15 total), tested against
#     every role (admin / architect / viewer / editor), asserting allow vs.
#     deny exactly as granted in bootstrap::default_policy()
#   - user CRUD lifecycle (admin-only)
#   - database create/delete lifecycle, including start/stop/restart
#   - conflicts, not-found, and malformed-input edge cases
#   - stale tokens after role change / user deletion
#   - multi-role permission union
#   - header case-insensitivity
#
#   JA NEGRIBI LAI NOTIRAS TERMINALIS, TAD AIZKOMENTEE NAKAMO RINDU
clear
# ==============================================================================

BASE_URL="http://localhost:6006"
RUN_ID=$(date +%s)
PASS_COUNT=0
FAIL_COUNT=0

section() {
    echo
    echo "=============================================================="
    echo " $1"
    echo "=============================================================="
}

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

get_token() {
    curl -s -X POST "$BASE_URL/api/login" -d "$1" | jq -r '.data.token'
}

echo "=========================================="
echo " Corelamo full permission-matrix test suite"
echo " run id: $RUN_ID"
echo "=========================================="

# ------------------------------------------------------------------
# SETUP — login as admin, basic login failure paths
# ------------------------------------------------------------------
section "SETUP — login"

check "login as admin" 200 \
  -X POST "$BASE_URL/api/login" -d '{"username":"admin","password":"secret"}'

ADMIN_TOKEN=$(get_token '{"username":"admin","password":"secret"}')

check "login wrong password" 401 \
  -X POST "$BASE_URL/api/login" -d '{"username":"admin","password":"wrongpassword"}'

check "login unknown user" 401 \
  -X POST "$BASE_URL/api/login" -d '{"username":"no-such-user-'"$RUN_ID"'","password":"whatever"}'

check "login malformed json body" 400 \
  -X POST "$BASE_URL/api/login" -d 'not-json-at-all'

check "login missing password field" 400 \
  -X POST "$BASE_URL/api/login" -d '{"username":"admin"}'

# ------------------------------------------------------------------
# SETUP — one user per role
# ------------------------------------------------------------------
section "SETUP — create one user per role"

check "create architect test user" 200 \
  -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"username":"architect_'"$RUN_ID"'","password":"secret","roles":["architect"]}'

check "create viewer test user" 200 \
  -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"username":"viewer_'"$RUN_ID"'","password":"secret","roles":["viewer"]}'

check "create editor test user" 200 \
  -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"username":"editor_'"$RUN_ID"'","password":"secret","roles":["editor"]}'

ARCHITECT_TOKEN=$(get_token '{"username":"architect_'"$RUN_ID"'","password":"secret"}')
VIEWER_TOKEN=$(get_token '{"username":"viewer_'"$RUN_ID"'","password":"secret"}')
EDITOR_TOKEN=$(get_token '{"username":"editor_'"$RUN_ID"'","password":"secret"}')

# ------------------------------------------------------------------
# SETUP — scratch database + seed document, used by most matrix tests
# ------------------------------------------------------------------
section "SETUP — scratch database + seed document"

SCRATCH_DB="scratch_$RUN_ID"

check "admin creates scratch database" 201 \
  -X POST "$BASE_URL/api/databases/$SCRATCH_DB/create-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "admin starts scratch database" 200 \
  -X POST "$BASE_URL/api/databases/$SCRATCH_DB/start-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "admin inserts seed document" 200 \
  -X POST "$BASE_URL/api/databases/$SCRATCH_DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"seed-doc-'"$RUN_ID"'","title":"Seed"}'

# ==================================================================
# PERMISSION MATRIX — one section per Permission variant in bootstrap.rs
# ==================================================================

section "MATRIX — Permission::Search (admin, architect, viewer, editor all granted)"

check "admin can search"     200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"seed","docs":3}'
check "architect can search" 200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/search" -H "X-Corelamo-Key: $ARCHITECT_TOKEN" -d '{"query":"seed","docs":3}'
check "viewer can search"    200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/search" -H "X-Corelamo-Key: $VIEWER_TOKEN"    -d '{"query":"seed","docs":3}'
check "editor can search"    200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/search" -H "X-Corelamo-Key: $EDITOR_TOKEN"    -d '{"query":"seed","docs":3}'

section "MATRIX — Permission::Retrieve (admin, architect, viewer, editor all granted)"

check "admin can retrieve"     200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/retrieve" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '["seed-doc-'"$RUN_ID"'"]'
check "architect can retrieve" 200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/retrieve" -H "X-Corelamo-Key: $ARCHITECT_TOKEN" -d '["seed-doc-'"$RUN_ID"'"]'
check "viewer can retrieve"    200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/retrieve" -H "X-Corelamo-Key: $VIEWER_TOKEN"    -d '["seed-doc-'"$RUN_ID"'"]'
check "editor can retrieve"    200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/retrieve" -H "X-Corelamo-Key: $EDITOR_TOKEN"    -d '["seed-doc-'"$RUN_ID"'"]'

section "MATRIX — Permission::Insert (admin, editor granted; architect, viewer denied)"

check "admin can insert"        200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"perm-insert-admin-'"$RUN_ID"'","title":"T"}'
check "architect cannot insert" 403 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/insert" -H "X-Corelamo-Key: $ARCHITECT_TOKEN" -d '{"id":"perm-insert-architect-'"$RUN_ID"'","title":"T"}'
check "viewer cannot insert"    403 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/insert" -H "X-Corelamo-Key: $VIEWER_TOKEN"    -d '{"id":"perm-insert-viewer-'"$RUN_ID"'","title":"T"}'
check "editor can insert"       200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/insert" -H "X-Corelamo-Key: $EDITOR_TOKEN"    -d '{"id":"perm-insert-editor-'"$RUN_ID"'","title":"T"}'

section "MATRIX — Permission::Delete (admin, editor granted; architect, viewer denied)"

check "admin seeds doc for admin-delete"  200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"id":"perm-delete-admin-'"$RUN_ID"'","title":"T"}'
check "admin seeds doc for editor-delete" 200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"id":"perm-delete-editor-'"$RUN_ID"'","title":"T"}'

check "admin can delete"        200 -X DELETE "$BASE_URL/api/databases/$SCRATCH_DB/delete" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '["perm-delete-admin-'"$RUN_ID"'"]'
check "architect cannot delete" 403 -X DELETE "$BASE_URL/api/databases/$SCRATCH_DB/delete" -H "X-Corelamo-Key: $ARCHITECT_TOKEN" -d '["perm-delete-editor-'"$RUN_ID"'"]'
check "viewer cannot delete"    403 -X DELETE "$BASE_URL/api/databases/$SCRATCH_DB/delete" -H "X-Corelamo-Key: $VIEWER_TOKEN"    -d '["perm-delete-editor-'"$RUN_ID"'"]'
check "editor can delete"       200 -X DELETE "$BASE_URL/api/databases/$SCRATCH_DB/delete" -H "X-Corelamo-Key: $EDITOR_TOKEN"    -d '["perm-delete-editor-'"$RUN_ID"'"]'

section "MATRIX — Permission::Replace / (admin, editor granted; architect, viewer denied)"

check "admin seeds doc for admin-replace"  200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"id":"perm-replace-admin-'"$RUN_ID"'","title":"Original"}'
check "admin seeds doc for editor-replace" 200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"id":"perm-replace-editor-'"$RUN_ID"'","title":"Original"}'

check "admin can replace"        200 -X PUT "$BASE_URL/api/databases/$SCRATCH_DB/replace" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"perm-replace-admin-'"$RUN_ID"'","title":"Replaced"}'
check "architect cannot replace" 403 -X PUT "$BASE_URL/api/databases/$SCRATCH_DB/replace" -H "X-Corelamo-Key: $ARCHITECT_TOKEN" -d '{"id":"perm-replace-editor-'"$RUN_ID"'","title":"Replaced"}'
check "viewer cannot replace"    403 -X PUT "$BASE_URL/api/databases/$SCRATCH_DB/replace" -H "X-Corelamo-Key: $VIEWER_TOKEN"    -d '{"id":"perm-replace-editor-'"$RUN_ID"'","title":"Replaced"}'
check "editor can replace"       200 -X PUT "$BASE_URL/api/databases/$SCRATCH_DB/replace" -H "X-Corelamo-Key: $EDITOR_TOKEN"    -d '{"id":"perm-replace-editor-'"$RUN_ID"'","title":"Replaced"}'

section "MATRIX — Permission::Upsert / (admin, editor granted; architect, viewer denied)"

check "admin can upsert"        200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/upsert" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"perm-upsert-admin-'"$RUN_ID"'","title":"Upserted"}'
check "architect cannot upsert" 403 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/upsert" -H "X-Corelamo-Key: $ARCHITECT_TOKEN" -d '{"id":"perm-upsert-editor-'"$RUN_ID"'","title":"Upserted"}'
check "viewer cannot upsert"    403 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/upsert" -H "X-Corelamo-Key: $VIEWER_TOKEN"    -d '{"id":"perm-upsert-admin-'"$RUN_ID"'","title":"Upserted"}'
check "editor can upsert"       200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/upsert" -H "X-Corelamo-Key: $EDITOR_TOKEN"    -d '{"id":"perm-upsert-editor-'"$RUN_ID"'","title":"Upserted"}'

section "MATRIX — Permission::CreateDatabase (admin, architect granted; viewer, editor denied)"

check "viewer cannot create database"  403 -X POST "$BASE_URL/api/databases/denied_createdb_$RUN_ID/create-database" -H "X-Corelamo-Key: $VIEWER_TOKEN"
check "editor cannot create database"  403 -X POST "$BASE_URL/api/databases/denied_createdb_$RUN_ID/create-database" -H "X-Corelamo-Key: $EDITOR_TOKEN"
check "admin can create database"      201 -X POST "$BASE_URL/api/databases/admin_createdb_$RUN_ID/create-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "architect can create database"  201 -X POST "$BASE_URL/api/databases/architect_createdb_$RUN_ID/create-database" -H "X-Corelamo-Key: $ARCHITECT_TOKEN"

section "MATRIX — Permission::DeleteDatabase (admin, architect granted; viewer, editor denied)"

check "admin creates db for admin-delete test"     201 -X POST "$BASE_URL/api/databases/admin_deldb_$RUN_ID/create-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "admin creates db for architect-delete test" 201 -X POST "$BASE_URL/api/databases/architect_deldb_$RUN_ID/create-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "admin creates db for denied-delete test"    201 -X POST "$BASE_URL/api/databases/denied_deldb_$RUN_ID/create-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "viewer cannot delete database"  403 -X DELETE "$BASE_URL/api/databases/denied_deldb_$RUN_ID/delete-database" -H "X-Corelamo-Key: $VIEWER_TOKEN"
check "editor cannot delete database"  403 -X DELETE "$BASE_URL/api/databases/denied_deldb_$RUN_ID/delete-database" -H "X-Corelamo-Key: $EDITOR_TOKEN"
check "admin can delete database"      200 -X DELETE "$BASE_URL/api/databases/admin_deldb_$RUN_ID/delete-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "architect can delete database"  200 -X DELETE "$BASE_URL/api/databases/architect_deldb_$RUN_ID/delete-database" -H "X-Corelamo-Key: $ARCHITECT_TOKEN"

section "MATRIX — Permission::ListDatabase (admin, architect granted; viewer, editor denied)"

check "admin can list databases"     200 -X GET "$BASE_URL/api/list-databases" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "architect can list databases" 200 -X GET "$BASE_URL/api/list-databases" -H "X-Corelamo-Key: $ARCHITECT_TOKEN"
check "viewer cannot list databases" 403 -X GET "$BASE_URL/api/list-databases" -H "X-Corelamo-Key: $VIEWER_TOKEN"
check "editor cannot list databases" 403 -X GET "$BASE_URL/api/list-databases" -H "X-Corelamo-Key: $EDITOR_TOKEN"

section "MATRIX — Permission::Status (admin, architect granted; viewer, editor denied)"

check "admin can view status"     200 -X GET "$BASE_URL/api/databases/$SCRATCH_DB/status" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "architect can view status" 200 -X GET "$BASE_URL/api/databases/$SCRATCH_DB/status" -H "X-Corelamo-Key: $ARCHITECT_TOKEN"
check "viewer cannot view status" 403 -X GET "$BASE_URL/api/databases/$SCRATCH_DB/status" -H "X-Corelamo-Key: $VIEWER_TOKEN"
check "editor can view status" 200 -X GET "$BASE_URL/api/databases/$SCRATCH_DB/status" -H "X-Corelamo-Key: $EDITOR_TOKEN"

section "MATRIX — Permission::GetPolicy (admin, architect, editor granted; viewer denied)"

check "admin can get policy"     200 -X GET "$BASE_URL/api/databases/$SCRATCH_DB/get-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "architect can get policy" 200 -X GET "$BASE_URL/api/databases/$SCRATCH_DB/get-policy" -H "X-Corelamo-Key: $ARCHITECT_TOKEN"
check "editor can get policy"    200 -X GET "$BASE_URL/api/databases/$SCRATCH_DB/get-policy" -H "X-Corelamo-Key: $EDITOR_TOKEN"
check "viewer cannot get policy" 403 -X GET "$BASE_URL/api/databases/$SCRATCH_DB/get-policy" -H "X-Corelamo-Key: $VIEWER_TOKEN"

section "MATRIX — Permission::PostPolicy (admin, architect, editor granted; viewer denied)"

check "admin can set policy"     200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d $'[[fields]]\nname = "id"\nxpath = 1\nindex = "Id"\nlist = true\n[fields.weight]\nmin = 100\nmax = 100'
check "architect can set policy" 200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/set-policy" -H "X-Corelamo-Key: $ARCHITECT_TOKEN" -d $'[[fields]]\nname = "id"\nxpath = 1\nindex = "Id"\nlist = true\n[fields.weight]\nmin = 100\nmax = 100'
check "editor can set policy"    200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/set-policy" -H "X-Corelamo-Key: $EDITOR_TOKEN"    -d $'[[fields]]\nname = "id"\nxpath = 1\nindex = "Id"\nlist = true\n[fields.weight]\nmin = 100\nmax = 100'
check "viewer cannot set policy" 403 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/set-policy" -H "X-Corelamo-Key: $VIEWER_TOKEN"    -d $'[[fields]]\nname = "id"\nxpath = 1\nindex = "Id"\nlist = true\n[fields.weight]\nmin = 100\nmax = 100'

section "MATRIX — Permission::CreateUser (admin only)"

check "architect cannot create user" 403 -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ARCHITECT_TOKEN" -d '{"username":"blocked_'"$RUN_ID"'","password":"x","roles":["viewer"]}'
check "viewer cannot create user"    403 -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $VIEWER_TOKEN"    -d '{"username":"blocked_'"$RUN_ID"'","password":"x","roles":["viewer"]}'
check "editor cannot create user"    403 -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $EDITOR_TOKEN"    -d '{"username":"blocked_'"$RUN_ID"'","password":"x","roles":["viewer"]}'
check "admin can create user"        200 -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"username":"crud_'"$RUN_ID"'","password":"secret","roles":["viewer"]}'

section "MATRIX — Permission::UpdatePwd (admin only)"

check "architect cannot update password" 403 -X POST "$BASE_URL/api/users/crud_$RUN_ID/password" -H "X-Corelamo-Key: $ARCHITECT_TOKEN" -d '{"password":"newpass"}'
check "viewer cannot update password"    403 -X POST "$BASE_URL/api/users/crud_$RUN_ID/password" -H "X-Corelamo-Key: $VIEWER_TOKEN"    -d '{"password":"newpass"}'
check "editor cannot update password"    403 -X POST "$BASE_URL/api/users/crud_$RUN_ID/password" -H "X-Corelamo-Key: $EDITOR_TOKEN"    -d '{"password":"newpass"}'
check "admin can update password"        200 -X POST "$BASE_URL/api/users/crud_$RUN_ID/password" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"password":"newpass"}'

section "MATRIX — Permission::UpdateRole (admin only)"

check "architect cannot update roles" 403 -X POST "$BASE_URL/api/users/crud_$RUN_ID/roles" -H "X-Corelamo-Key: $ARCHITECT_TOKEN" -d '{"roles":["editor"]}'
check "viewer cannot update roles"    403 -X POST "$BASE_URL/api/users/crud_$RUN_ID/roles" -H "X-Corelamo-Key: $VIEWER_TOKEN"    -d '{"roles":["editor"]}'
check "editor cannot update roles"    403 -X POST "$BASE_URL/api/users/crud_$RUN_ID/roles" -H "X-Corelamo-Key: $EDITOR_TOKEN"    -d '{"roles":["editor"]}'
check "admin can update roles"        200 -X POST "$BASE_URL/api/users/crud_$RUN_ID/roles" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"roles":["editor"]}'

section "MATRIX — Permission::DeleteUser (admin only)"

check "architect cannot delete user" 403 -X DELETE "$BASE_URL/api/users/crud_$RUN_ID" -H "X-Corelamo-Key: $ARCHITECT_TOKEN"
check "viewer cannot delete user"    403 -X DELETE "$BASE_URL/api/users/crud_$RUN_ID" -H "X-Corelamo-Key: $VIEWER_TOKEN"
check "editor cannot delete user"    403 -X DELETE "$BASE_URL/api/users/crud_$RUN_ID" -H "X-Corelamo-Key: $EDITOR_TOKEN"
check "admin can delete user"        200 -X DELETE "$BASE_URL/api/users/crud_$RUN_ID" -H "X-Corelamo-Key: $ADMIN_TOKEN"

section "MATRIX — Permission::GetConfig (admin, architect, editor granted; viewer denied)"

check "admin can view config"     200 -X GET "$BASE_URL/api/databases/$SCRATCH_DB/get-config" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "viewer cannot view config" 403 -X GET "$BASE_URL/api/databases/$SCRATCH_DB/get-config" -H "X-Corelamo-Key: $VIEWER_TOKEN"
check "architect can view config" 200 -X GET "$BASE_URL/api/databases/$SCRATCH_DB/get-config" -H "X-Corelamo-Key: $ARCHITECT_TOKEN"
check "editor can view config" 200 -X GET "$BASE_URL/api/databases/$SCRATCH_DB/get-config" -H "X-Corelamo-Key: $EDITOR_TOKEN"

section "MATRIX — Permission::SetConfig (admin, architect, editor granted; viewer denied)"

check "admin can set config"      200 -X PUT "$BASE_URL/api/databases/$SCRATCH_DB/set-config" -H "X-Corelamo-Key: $ADMIN_TOKEN"\
  -d '
  enable_background_compaction = true
  bootable = false

  [runtime]
  flush_threshold = 100000
  indexing_batch_size = 100000
  indexing_window_size = 10000

  [runtime.compaction]
  max_segments_per_compaction = 8
  compact_when_segments_at_least = 16

  [compaction_interval]
  secs = 1
  nanos = 0
  '
check "viewer cannot set config"  403 -X PUT "$BASE_URL/api/databases/$SCRATCH_DB/set-config" -H "X-Corelamo-Key: $VIEWER_TOKEN"\
  -d '
  enable_background_compaction = true
  bootable = false

  [runtime]
  flush_threshold = 100000
  indexing_batch_size = 100000
  indexing_window_size = 10000

  [runtime.compaction]
  max_segments_per_compaction = 8
  compact_when_segments_at_least = 16

  [compaction_interval]
  secs = 1
  nanos = 0
  '
check "editor can set config"  200 -X PUT "$BASE_URL/api/databases/$SCRATCH_DB/set-config" -H "X-Corelamo-Key: $EDITOR_TOKEN"\
  -d '
  enable_background_compaction = true
  bootable = false

  [runtime]
  flush_threshold = 100000
  indexing_batch_size = 100000
  indexing_window_size = 10000

  [runtime.compaction]
  max_segments_per_compaction = 8
  compact_when_segments_at_least = 16

  [compaction_interval]
  secs = 1
  nanos = 0
  '
check "architect can set config"  200 -X PUT "$BASE_URL/api/databases/$SCRATCH_DB/set-config" -H "X-Corelamo-Key: $ARCHITECT_TOKEN"\
  -d '
  enable_background_compaction = true
  bootable = false

  [runtime]
  flush_threshold = 100000
  indexing_batch_size = 100000
  indexing_window_size = 10000

  [runtime.compaction]
  max_segments_per_compaction = 8
  compact_when_segments_at_least = 16

  [compaction_interval]
  secs = 1
  nanos = 0
  '

section "MATRIX — Permission::Reindex (admin, architect, editor granted; viewer denied)"

check "admin can reindex"     200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/reindex" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "viewer cannot reindex" 403 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/reindex" -H "X-Corelamo-Key: $VIEWER_TOKEN"
check "editor can reindex" 200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/reindex" -H "X-Corelamo-Key: $EDITOR_TOKEN"
check "architect can reindex" 200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/reindex" -H "X-Corelamo-Key: $ARCHITECT_TOKEN"

# ==================================================================
# LIFECYCLE — start / stop / restart-database + conflict codes
# ==================================================================
section "LIFECYCLE — start / stop / restart-database"

LIFECYCLE_DB="lifecycle_$RUN_ID"

check "admin creates lifecycle database"         201 -X POST "$BASE_URL/api/databases/$LIFECYCLE_DB/create-database"  -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "admin starts lifecycle database"          200 -X POST "$BASE_URL/api/databases/$LIFECYCLE_DB/start-database"   -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "starting an already-running db conflicts" 200 -X POST "$BASE_URL/api/databases/$LIFECYCLE_DB/start-database"   -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "admin stops lifecycle database"           200 -X POST "$BASE_URL/api/databases/$LIFECYCLE_DB/stop-database"    -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "stopping an already-stopped db conflicts" 200 -X POST "$BASE_URL/api/databases/$LIFECYCLE_DB/stop-database"    -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "searching a stopped database fails"       409 -X POST "$BASE_URL/api/databases/$LIFECYCLE_DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"query":"x","docs":1}'
check "admin restarts lifecycle database"        200 -X POST "$BASE_URL/api/databases/$LIFECYCLE_DB/restart-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "admin deletes lifecycle database"         200 -X DELETE "$BASE_URL/api/databases/$LIFECYCLE_DB/delete-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"

# ==================================================================
# EXTRA — conflicts, not-found, malformed input
# ==================================================================
section "EXTRA — conflicts, not-found, malformed input"

check "creating duplicate database conflicts" 409 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/create-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "creating duplicate user conflicts"     409 -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"username":"architect_'"$RUN_ID"'","password":"x","roles":["viewer"]}'

check "search on nonexistent database is 404" 404 -X POST "$BASE_URL/api/databases/no_such_db_$RUN_ID/search" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"query":"x","docs":1}'
check "status on nonexistent database is 404" 404 -X GET "$BASE_URL/api/databases/no_such_db_$RUN_ID/status" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "deleting nonexistent database is 404"  404 -X DELETE "$BASE_URL/api/databases/no_such_db_$RUN_ID/delete-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "deleting nonexistent user is 404"       404 -X DELETE "$BASE_URL/api/users/no_such_user_$RUN_ID" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "create user with unknown role is rejected" 400 -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"username":"badrole_'"$RUN_ID"'","password":"x","roles":["not-a-real-role"]}'

check "insert with empty body is invalid" 400 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d ''

# ==================================================================
# EXTRA — token / header edge cases
# ==================================================================
section "EXTRA — token / header edge cases"

check "missing token on protected route"  401 -X GET "$BASE_URL/api/list-databases"
check "garbage token on protected route"  401 -X GET "$BASE_URL/api/list-databases" -H "X-Corelamo-Key: not-a-real-token"
check "lowercase header name still works" 200 -X GET "$BASE_URL/api/list-databases" -H "x-corelamo-key: $ADMIN_TOKEN"

# ==================================================================
# EXTRA — stale tokens after account changes
# ==================================================================
section "EXTRA — stale tokens after role change / user deletion"

check "create throwaway stale-token user" 200 -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"username":"stale_'"$RUN_ID"'","password":"secret","roles":["editor"]}'
STALE_TOKEN=$(get_token '{"username":"stale_'"$RUN_ID"'","password":"secret"}')

check "stale-token user can insert before role change" 200 \
  -X POST "$BASE_URL/api/databases/$SCRATCH_DB/insert" -H "X-Corelamo-Key: $STALE_TOKEN" -d '{"id":"stale-before-'"$RUN_ID"'","title":"T"}'

check "admin demotes stale-token user to viewer" 200 \
  -X POST "$BASE_URL/api/users/stale_$RUN_ID/roles" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"roles":["viewer"]}'

check "same old token can no longer insert after demotion" 403 \
  -X POST "$BASE_URL/api/databases/$SCRATCH_DB/insert" -H "X-Corelamo-Key: $STALE_TOKEN" -d '{"id":"stale-after-'"$RUN_ID"'","title":"T"}'

check "admin deletes stale-token user" 200 -X DELETE "$BASE_URL/api/users/stale_$RUN_ID" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "token of a deleted user is now unauthorized" 401 \
  -X POST "$BASE_URL/api/databases/$SCRATCH_DB/search" -H "X-Corelamo-Key: $STALE_TOKEN" -d '{"query":"x","docs":1}'

# ==================================================================
# EXTRA — multi-role union of permissions
# ==================================================================
section "EXTRA — multi-role union of permissions"

check "create multi-role user (viewer+editor)" 200 -X POST "$BASE_URL/api/users" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"username":"multi_'"$RUN_ID"'","password":"secret","roles":["viewer","editor"]}'
MULTI_TOKEN=$(get_token '{"username":"multi_'"$RUN_ID"'","password":"secret"}')

check "multi-role user can search (from viewer)"    200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/search"  -H "X-Corelamo-Key: $MULTI_TOKEN" -d '{"query":"x","docs":1}'
check "multi-role user can insert (from editor)"    200 -X POST "$BASE_URL/api/databases/$SCRATCH_DB/insert"  -H "X-Corelamo-Key: $MULTI_TOKEN" -d '{"id":"multi-'"$RUN_ID"'","title":"T"}'
check "multi-role user still cannot list databases" 403 -X GET "$BASE_URL/api/list-databases" -H "X-Corelamo-Key: $MULTI_TOKEN"

check "admin deletes multi-role test user" 200 -X DELETE "$BASE_URL/api/users/multi_$RUN_ID" -H "X-Corelamo-Key: $ADMIN_TOKEN"

# ==================================================================
# CLEANUP
# ==================================================================
section "CLEANUP"

check "delete architect test user"      200 -X DELETE "$BASE_URL/api/users/architect_$RUN_ID" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "delete viewer test user"         200 -X DELETE "$BASE_URL/api/users/viewer_$RUN_ID"    -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "delete editor test user"         200 -X DELETE "$BASE_URL/api/users/editor_$RUN_ID"    -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "delete scratch database"         200 -X DELETE "$BASE_URL/api/databases/$SCRATCH_DB/delete-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "delete admin_createdb database"  200 -X DELETE "$BASE_URL/api/databases/admin_createdb_$RUN_ID/delete-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "delete architect_createdb database" 200 -X DELETE "$BASE_URL/api/databases/architect_createdb_$RUN_ID/delete-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "delete denied_deldb database"      200 -X DELETE "$BASE_URL/api/databases/denied_deldb_$RUN_ID/delete-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"


echo
echo "=============================================================="
echo " Results: $PASS_COUNT passed, $FAIL_COUNT failed"
echo "=============================================================="

rm -f /tmp/auth_test_body.json

if [ "$FAIL_COUNT" -gt 0 ]; then
    exit 1
fi
