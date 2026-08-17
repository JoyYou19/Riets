#!/bin/bash
# ==============================================================================
# HOW TO USE:
#   1. Start the server first:  cargo run -p core-runtime -- --root-path /tmp
#   2. Run this whole file:     bash backup_restore_test.sh
# ==============================================================================
clear
BASE_URL="http://localhost:6006"
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
    response=$(curl -s -o /tmp/backup_test_body.json -w "%{http_code}" "$@")

    if [ "$response" == "$expected" ]; then
        echo "PASS  [$label] got $response"
        PASS_COUNT=$((PASS_COUNT + 1))
    else
        echo "FAIL  [$label] expected $expected, got $response"
        echo "      body: $(cat /tmp/backup_test_body.json)"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
}

# usage: assert_same "<label>" <file_a> <file_b>
# Compares only the .data field so per-request noise (request_id, time_taken)
# doesn't cause false failures.
assert_same() {
    local label="$1" file_a="$2" file_b="$3"
    local data_a data_b
    data_a=$(jq -S '.data' "$file_a" 2>/dev/null)
    data_b=$(jq -S '.data' "$file_b" 2>/dev/null)
    if [ "$data_a" == "$data_b" ]; then
        echo "PASS  [$label] data matches"
        PASS_COUNT=$((PASS_COUNT + 1))
    else
        echo "FAIL  [$label] data differs"
        echo "      $file_a: $data_a"
        echo "      $file_b: $data_b"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
}

# usage: assert_different "<label>" <file_a> <file_b>
# Compares only the .data field so per-request noise doesn't mask a real match.
assert_different() {
    local label="$1" file_a="$2" file_b="$3"
    local data_a data_b
    data_a=$(jq -S '.data' "$file_a" 2>/dev/null)
    data_b=$(jq -S '.data' "$file_b" 2>/dev/null)
    if [ "$data_a" == "$data_b" ]; then
        echo "FAIL  [$label] data is identical, expected a difference"
        echo "      body: $data_a"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    else
        echo "PASS  [$label] data differs as expected"
        PASS_COUNT=$((PASS_COUNT + 1))
    fi
}

get_token() {
    curl -s -X POST "$BASE_URL/api/login" -d "$1" | jq -r '.data.token'
}

retrieve_ids() {
    # captures the retrieve response for a fixed id set into the given file
    local outfile="$1"
    curl -s -X POST "$BASE_URL/api/databases/$DB/retrieve" \
        -H "X-Corelamo-Key: $ADMIN_TOKEN" \
        -d '["1","2","3","99"]' > "$outfile"
}

ADMIN_TOKEN=$(get_token '{"username":"admin","password":"secret"}')

echo "=========================================="
echo " Corelamo backup/restore test suite"
echo "=========================================="

DB="backup_test"
SHARD_COUNT=5
BACKUP_ROOT="/tmp/corelamo/databases/${DB}/backups"

# ------------------------------------------------------------------
# Setup
# ------------------------------------------------------------------
section "Database setup"

check_leftover_delete() {
    local response
    response=$(curl -s -o /tmp/backup_test_body.json -w "%{http_code}" -X DELETE "$BASE_URL/api/databases/$DB/delete-database" -H "X-Corelamo-Key: $ADMIN_TOKEN")
    if [ "$response" == "200" ] || [ "$response" == "404" ]; then
        echo "PASS  [delete leftover database] got $response (either is fine on a clean run)"
        PASS_COUNT=$((PASS_COUNT + 1))
    else
        echo "FAIL  [delete leftover database] got $response"
        echo "      body: $(cat /tmp/backup_test_body.json)"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
}

check_leftover_delete
check "create database" 201 -X POST "$BASE_URL/api/databases/$DB/create-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "start database" 200 -X POST "$BASE_URL/api/databases/$DB/start-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "set policy" 200 -X POST "$BASE_URL/api/databases/$DB/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '
  [[fields]]
  name = "id"
  xpath = 0
  index = "Id"
  list = true
  [fields.weight]
  min = 100
  max = 100

  [[fields]]
  name = "number"
  xpath = 1
  index = "Text"
  list = true
  [fields.weight]
  min = 100
  max = 100
  '

# ------------------------------------------------------------------
# Seed baseline documents
# ------------------------------------------------------------------
section "Seed baseline documents"

check "insert doc 1" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"id":"1","number":"one"}'
check "insert doc 2" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"id":"2","number":"two"}'
check "insert doc 3" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"id":"3","number":"three"}'

# capture the pre-backup state — this is what restore should bring us back to.
# doc "99" is included in the id set but doesn't exist yet; it's added after
# backup, so its absence here is part of what confirms restore reverted it.
retrieve_ids /tmp/backup_test_baseline.json
echo "  baseline captured (docs 1,2,3 present, 99 absent)"

# ------------------------------------------------------------------
# Backup
# ------------------------------------------------------------------
section "Create backup"

check "trigger backup" 200 -X POST "$BASE_URL/api/databases/$DB/backup" -H "X-Corelamo-Key: $ADMIN_TOKEN"

# The backup handler responds as soon as the job is spawned, not when the
# files are actually written, so poll disk until every shard's backup
# folder has a manifest before continuing.
TIMEOUT_SECONDS=300
POLL_INTERVAL=2
DEADLINE=$(( $(date +%s) + TIMEOUT_SECONDS ))
echo "  waiting for backup files to appear in all $SHARD_COUNT shard folders..."

declare -a SHARD_DONE
for i in $(seq 0 $((SHARD_COUNT - 1))); do SHARD_DONE[$i]=""; done

while true; do
    ALL_READY=true
    for i in $(seq 0 $((SHARD_COUNT - 1))); do
        [ -n "${SHARD_DONE[$i]}" ] && continue
        SHARD_DIR="${BACKUP_ROOT}/shard-${i}"
        LATEST=$(ls -1 "$SHARD_DIR" 2>/dev/null | grep '^full_' | sort | tail -n1 || true)
        if [ -n "$LATEST" ] && [ -f "${SHARD_DIR}/${LATEST}/manifest.json" ] \
           && [ -f "${SHARD_DIR}/${LATEST}/documents.bin.br" ] \
           && [ -d "${SHARD_DIR}/${LATEST}/index" ]; then
            SHARD_DONE[$i]="$LATEST"
            echo "  shard-${i}: backup ready ($LATEST)"
        else
            ALL_READY=false
        fi
    done
    [ "$ALL_READY" = true ] && break
    if [ "$(date +%s)" -ge "$DEADLINE" ]; then
        echo "FAIL  [backup files on disk] timed out after ${TIMEOUT_SECONDS}s"
        FAIL_COUNT=$((FAIL_COUNT + 1))
        break
    fi
    sleep "$POLL_INTERVAL"
done
echo "PASS  [backup files on disk] all $SHARD_COUNT shards confirmed"
PASS_COUNT=$((PASS_COUNT + 1))

# ------------------------------------------------------------------
# Mutate after backup
# ------------------------------------------------------------------
section "Mutate after backup"

check "delete doc 2" 200 -X DELETE "$BASE_URL/api/databases/$DB/delete" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '["2"]'
check "insert doc 99" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"id":"99","number":"ninety-nine"}'

retrieve_ids /tmp/backup_test_mutated.json
echo "  mutated state captured (doc 2 deleted, doc 99 inserted)"

assert_different "mutation actually changed state" /tmp/backup_test_baseline.json /tmp/backup_test_mutated.json

# ------------------------------------------------------------------
# Restore
# ------------------------------------------------------------------
section "Restore backup"

check "trigger restore" 200 -X POST "$BASE_URL/api/databases/$DB/restore-backup" -H "X-Corelamo-Key: $ADMIN_TOKEN"

sleep 2  # small buffer in case restore write-back isn't instantly visible
retrieve_ids /tmp/backup_test_restored.json
echo "  post-restore state captured"

assert_same "restore reverted to baseline" /tmp/backup_test_baseline.json /tmp/backup_test_restored.json

# ------------------------------------------------------------------
# Cleanup
# ------------------------------------------------------------------
section "CLEANUP"

check "delete database" 200 -X DELETE "$BASE_URL/api/databases/$DB/delete-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"

echo
echo "=============================================================="
echo " Results: $PASS_COUNT passed, $FAIL_COUNT failed"
echo "=============================================================="

rm -f /tmp/backup_test_body.json /tmp/backup_test_baseline.json /tmp/backup_test_mutated.json /tmp/backup_test_restored.json

if [ "$FAIL_COUNT" -gt 0 ]; then
    exit 1
fi