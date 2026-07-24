#!/bin/bash
# ==============================================================================
# HOW TO USE:
#   1. Start the server first:  cargo run -p core-runtime -- --root-path /tmp
#   2. Run this whole file:     bash search_test.sh
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
        echo "PASS  [$label] got '$actual'"
        PASS_COUNT=$((PASS_COUNT + 1))
    else
        echo "FAIL  [$label] expected '$expected', got '$actual'"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
}

# usage: check_json_bool "<label>" '<jq boolean filter>'
check_json_bool() {
    local label="$1"
    local filter="$2"

    if jq -e "$filter" /tmp/search_test_body.json > /dev/null 2>&1; then
        echo "PASS  [$label]"
        PASS_COUNT=$((PASS_COUNT + 1))
    else
        echo "FAIL  [$label]"
        echo "      body: $(cat /tmp/search_test_body.json)"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
}

get_token() {
    curl -s -X POST "$BASE_URL/api/login" -d "$1" | jq -r '.data.token'
}

ADMIN_TOKEN=$(get_token '{"username":"admin","password":"secret"}')

echo "=========================================="
echo " Corelamo search command test suite"
echo " run id: $RUN_ID"
echo "=========================================="

# ------------------------------------------------------------------
# Database setup
# ------------------------------------------------------------------

DB="search_$RUN_ID"

check "create search database" 201 \
  -X POST "$BASE_URL/api/databases/$DB/create-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "start search database" 200 \
  -X POST "$BASE_URL/api/databases/$DB/start-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "insert seed document" 200 \
  -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"1","title":"real"}'

check "insert seed document" 200 \
  -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"2","title":"case"}'

check "insert seed document" 200 \
  -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"3","title":"CASE"}'

check "insert seed document" 200 \
  -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"4","title":"CaSe"}'

# ------------------------------------------------------------------
# Search query tests
# ------------------------------------------------------------------
section "Single search term"

check "non existing search term"    200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"nonexistent","docs":1}'
check_json_bool "data should be empty" '(.data | length) == 0'

check "existing search term"        200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"real","docs":1}'
check_json_bool "data should be one" '(.data | length) == 1'

check "existing search term case"        200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"case","docs":3}'
check_json_bool "data should be empty" '(.data | length) == 3'


# ------------------------------------------------------------------
# Cleanup
# ------------------------------------------------------------------
section "CLEANUP"

check "delete search database"      200 -X DELETE "$BASE_URL/api/databases/$DB/delete-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"


echo
echo "=============================================================="
echo " Results: $PASS_COUNT passed, $FAIL_COUNT failed"
echo "=============================================================="

rm -f /tmp/search_test_body.json

if [ "$FAIL_COUNT" -gt 0 ]; then
    exit 1
fi