#!/bin/bash
# ==============================================================================
# HOW TO USE:
#   1. Start the server first:  cargo run -p core-runtime -- --root-path /tmp
#   2. Run this whole file:     bash search_test.sh
#
#   JA NEGRIBI LAI NOTIRAS TERMINALIS, TAD AIZKOMENTEE NAKAMO RINDU
clear
# ==============================================================================

#Todo: special symbols (kad configa vares ielikt)

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
echo " Corelamo search command test suite"
echo " run id: $RUN_ID"
echo "=========================================="

# ------------------------------------------------------------------
# Database setup
# ------------------------------------------------------------------

DB="search_test"

check "create search database" 201 -X POST "$BASE_URL/api/databases/$DB/create-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "start search database" 200 -X POST "$BASE_URL/api/databases/$DB/start-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "set policy" 200 -X POST "$BASE_URL/api/databases/$DB/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
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
  name = "title"
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

check "insert seed document" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"title":"real"}'
check "insert seed document" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"title":"case"}'
check "insert seed document" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"title":"CASE"}'
check "insert seed document" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"title":"CaSe"}'

# ------------------------------------------------------------------
# Search query tests
# ------------------------------------------------------------------

section "Basic issues"

check "no document" 400 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "no query at all" 400 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{}'
check "bad json" 400 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d 'bad json'
check "empty query" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":""}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "empty query(stopword)"    200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"a"}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "empty query(stopwords)"    200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"a an the and or of is it this that he she you i am are was were be been being to in on for with as by at from but not his her their they we my your our who what when where why how"}'
    check_json_bool "data should be 0" '(.data | length) == 0'

section "Single search term"

check "non existing search term" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"notreal"}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "existing search term" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"real"}'
    check_json_bool "data should be 1" '(.data | length) == 1'
check "existing search term case insensitivity" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"case"}'
    check_json_bool "data should be 3" '(.data | length) == 3'
check "existing search term + regular symbol" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"real."}'
    check_json_bool "data should be 1" '(.data | length) == 1'
check "existing search term + regular symbol" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"#&@%$real.$&%(#*%@#&"}'
    check_json_bool "data should be 1" '(.data | length) == 1'
check "existing search term + regular symbol" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"$@real.[\\[]]"}'
    check_json_bool "data should be 1" '(.data | length) == 1'
check "existing search term divided by regular symbol" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"re.al."}'
    check_json_bool "data should be 0" '(.data | length) == 0'

#check "existing search term + specsymbol"        200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"real_","docs":3}'
#check_json_bool "data should be 0" '(.data | length) == 0'


section "AND"

#AND un PHRASE seeding
#...........
check "insert seed document" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"title":"real confirmed"}'
check "insert seed document" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"title":"nothing seems real"}'
  check "insert seed document" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"title":"nothing seems really real"}'
check "insert seed document" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"title":"real word word word confirmed"}'
check "insert seed document" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"title":"confirmed real"}'
  check "insert seed document" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"title":"word"}'
#...........

check "reindex" 200 -X POST "$BASE_URL/api/databases/$DB/reindex" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "non existing search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":" inserted"}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "OR existing search term" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"reality real"}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "OR existing search term" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"case real"}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "OR existing search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"everything seems real"}'
    check_json_bool "data should be 0" '(.data | length) == 0'

check "existing 2 search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"real confirmed"}'
    check_json_bool "data should be 3" '(.data | length) == 3'
check "existing 3 search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"nothing seems real"}'
    check_json_bool "data should be 2" '(.data | length) == 2'
check "existing 3 search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"seems nothing real"}'
    check_json_bool "data should be 2" '(.data | length) == 2'
check "two of the same" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"word word"}'
    check_json_bool "data should be 2" '(.data | length) == 2'
check "existing search terms + symbols" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":".nothing&*(#&seems.real"}'
    check_json_bool "data should be 2" '(.data | length) == 2'

#PASLAIK NESTRADA
section "Phrase"

check "non existing search term" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\"reality real\""}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "existing but separated search term" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\"nothing real\""}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "missing repeated word in middle" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\"real word confirmed\""}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "two of the same" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\"word word\""}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "wrong order" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\"real seems\""}'
    check_json_bool "data should be 0" '(.data | length) == 0'

check "existing 2 search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\"real confirmed\""}'
    check_json_bool "data should be 1" '(.data | length) == 1'
check "existing 3 search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\"nothing seems real\""}'
    check_json_bool "data should be 1" '(.data | length) == 1'
check "existing search terms + symbols" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\".nothing&*(#&seems.real\""}'
    check_json_bool "data should be 1" '(.data | length) == 1'

section "Exact match"
# ------------------------------------------------------------------
# Cleanup
# ------------------------------------------------------------------
section "CLEANUP"

check "delete search database" 200 -X DELETE "$BASE_URL/api/databases/$DB/delete-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"


echo
echo "=============================================================="
echo " Results: $PASS_COUNT passed, $FAIL_COUNT failed"
echo "=============================================================="

rm -f /tmp/search_test_body.json

if [ "$FAIL_COUNT" -gt 0 ]; then
    exit 1
fi