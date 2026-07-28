#!/bin/bash
# ==============================================================================
# HOW TO USE:
#   1. Start the server first:  cargo run -p core-runtime -- --root-path /tmp
#   2. Run this whole file:     bash search_test.sh
#
#   JA NEGRIBI LAI NOTIRAS TERMINALIS, TAD AIZKOMENTEE NAKAMO RINDU
clear
# ==============================================================================
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
    response=$(curl -s -o /tmp/search_test_body.json -w "%{http_code}" "$@")

    if [ "$response" == "$expected" ]; then
        echo "PASS  [$label] got $response"
        PASS_COUNT=$((PASS_COUNT + 1))
    else
        echo "FAIL  [$label] expected $expected, got $response"
        echo "      body: $(cat /tmp/search_test_body.json)"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
}

# usage: check_json "<label>" '<jq filter>' "<expected value>"
# Must be called immediately after a check(), since it reads the same
# temp file check() just wrote.
check_json() {
    local label="$1"
    local filter="$2"
    local expected="$3"
    local actual

    actual=$(jq -r "$filter" /tmp/search_test_body.json 2>/dev/null)

    if [ "$actual" == "$expected" ]; then
        echo "  PASS  [$label] got '$actual'"
        PASS_COUNT=$((PASS_COUNT + 1))
    else
        echo "  FAIL  [$label] expected '$expected', got '$actual'"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
}

# usage: check_json_bool "<label>" '<jq boolean filter>'
check_json_bool() {
    local label="$1"
    local filter="$2"

    if jq -e "$filter" /tmp/search_test_body.json > /dev/null 2>&1; then
        echo "  PASS  [$label]"
        PASS_COUNT=$((PASS_COUNT + 1))
    else
        echo "  FAIL  [$label]"
        echo "      body: $(cat /tmp/search_test_body.json)"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
}

get_token() {
    curl -s -X POST "$BASE_URL/api/login" -d "$1" | jq -r '.data.token'
}

ADMIN_TOKEN=$(get_token '{"username":"admin","password":"secret"}')

echo "=========================================="
echo " Corelamo modify command test suite"
echo " run id: $RUN_ID"
echo "=========================================="

# ------------------------------------------------------------------
# Database setup
# ------------------------------------------------------------------

DB="modify_test"

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

check "set config" 200 -X PUT "$BASE_URL/api/databases/$DB/config" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '
  enable_background_compaction = true
  bootable = false
  [runtime]
  flush_threshold = 100000
  indexing_batch_size = 100000
  [runtime.compaction]
  max_segments_per_compaction = 8
  compact_when_segments_at_least = 16
  [compaction_interval]
  secs = 1
  nanos = 0
  '

# ------------------------------------------------------------------
# Insert
# ------------------------------------------------------------------

section "Basic insert"

check "insert document without id" 409 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"number":"none"}'
check "insert document without id value" 409 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"", "number":"none"}'

check "insert normal document" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"1", "number":"1"}'
check "insert duplicate id" 409 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"1", "number":"none"}'

check "insert text id" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"bruh", "number":"2"}'
check "insert duplicate text id" 409 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"bruh", "number":"none"}'

check "insert document with two ids" 409 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"no","id":"way" "number":"none"}'
check "insert document with two number" 409 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"no","id":"way" "number":"none"}'

check "insert document with id 3" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"3", "number":"3"}'

section "Insert with autoincrement id"

check "set autoincrement policy" 200 -X POST "$BASE_URL/api/databases/$DB/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '
  [[fields]]
  name = "id"
  xpath = 0
  index = "IdAutoIncrement"
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

check "insert document with id value" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"4", "number":"4"}'
check "insert document with duplicate id value" 409 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"4", "number":"4"}'
check "insert document with id text value" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"yo", "number":"5"}'
check "insert document without id value" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"", "number":"6"}'
check "insert document without id" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"number":"7"}'
check "insert document with id" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"10", "number":"8 un 10"}'
check "insert document without id" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"number":"9"}'
check "insert document without id" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"number":"11"}'
# dazi te prosta lai saprastu ka autoincrement strada. number nozime kurus skaitlus dokuments aiznem autoincrementaa.
# ja insertee ar id tad i++, ka ari kad i nonak lidz sim skaitlim(ja tas ir skaitlis) tad to skipo.


# ------------------------------------------------------------------
# Replace
# ------------------------------------------------------------------

section "Replace"

check "replace non existing id" 409 -X PUT "$BASE_URL/api/databases/$DB/replace" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"idk","number":"none"}'
check "replace empty id" 409 -X PUT "$BASE_URL/api/databases/$DB/replace" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"","number":"none"}'
check "replace no id" 409 -X PUT "$BASE_URL/api/databases/$DB/replace" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"number":"one"}'
check "replace two ids" 409 -X PUT "$BASE_URL/api/databases/$DB/replace" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"1","id":"2","number":"onetwo"}'

check "replace existing id" 200 -X PUT "$BASE_URL/api/databases/$DB/replace" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"3","number":"three"}'


# ------------------------------------------------------------------
# Upsert
# ------------------------------------------------------------------
#velak kjipa bus ari partial replace funkcija

check "upsert an existing id" 200 -X PUT "$BASE_URL/api/databases/$DB/upsert" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"4","number":"four"}'
check "upsert a new id" 200 -X PUT "$BASE_URL/api/databases/$DB/upsert" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"what","number":"12"}'

# ------------------------------------------------------------------
# Delete
# ------------------------------------------------------------------



# ------------------------------------------------------------------
# Cleanup
# ------------------------------------------------------------------
section "CLEANUP"

check "delete database" 200 -X DELETE "$BASE_URL/api/databases/$DB/delete-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"


echo
echo "=============================================================="
echo " Results: $PASS_COUNT passed, $FAIL_COUNT failed"
echo "=============================================================="

rm -f /tmp/search_test_body.json

if [ "$FAIL_COUNT" -gt 0 ]; then
    exit 1
fi