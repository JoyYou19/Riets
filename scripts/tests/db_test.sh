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
echo " Corelamo search command test suite"
echo " run id: $RUN_ID"
echo "=========================================="


DB1="db_test1"
DB2="db_test2"


section "Create-database"

check "create no database" 404 -X POST "$BASE_URL/api/databases/create-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "create empty" 409 -X POST "$BASE_URL/api/databases//create-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "create database one" 201 -X POST "$BASE_URL/api/databases/$DB1/create-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "create duplicate database one" 409 -X POST "$BASE_URL/api/databases/$DB/create-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "create database two" 201 -X POST "$BASE_URL/api/databases/$DB2/create-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"


section "Start"

check "start no database" 404 -X POST "$BASE_URL/api/databases/start-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "start database empty" 404 -X POST "$BASE_URL/api/databases//start-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "start nonexistent database" 404 -X POST "$BASE_URL/api/databases/yo/start-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "start database one" 200 -X POST "$BASE_URL/api/databases/$DB1/start-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "start database one again" 200 -X POST "$BASE_URL/api/databases/$DB1/start-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "start database two" 200 -X POST "$BASE_URL/api/databases/$DB2/start-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"


section "Stop-database"

check "stop no database" 404 -X POST "$BASE_URL/api/databases/stop-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "stop database empty" 404 -X POST "$BASE_URL/api/databases//stop-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "stop nonexistent database" 404 -X POST "$BASE_URL/api/databases/yo/stop-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "stop database one" 200 -X POST "$BASE_URL/api/databases/$DB1/stop-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "stop database one again" 200 -X POST "$BASE_URL/api/databases/$DB1/stop-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "stop database two" 200 -X POST "$BASE_URL/api/databases/$DB2/stop-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"


section "Restart"

check "restart no database" 404 -X POST "$BASE_URL/api/databases/restart-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "restart database empty" 404 -X POST "$BASE_URL/api/databases//restart-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "restart nonexistent database" 404 -X POST "$BASE_URL/api/databases/yo/restart-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "restart database one(stopped)" 200 -X POST "$BASE_URL/api/databases/$DB1/restart-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "restart database one again(started)" 200 -X POST "$BASE_URL/api/databases/$DB1/restart-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"


section "Status"

check "status no database" 404 -X GET "$BASE_URL/api/databases/status" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "status database empty" 404 -X GET "$BASE_URL/api/databases//status" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "status nonexistent database" 404 -X GET "$BASE_URL/api/databases/yo/status" -H "X-Corelamo-Key: $ADMIN_TOKEN"


check "status database one(running)" 200 -X GET "$BASE_URL/api/databases/$DB1/status" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "status database two(stopped)" 409 -X GET "$BASE_URL/api/databases/$DB2/status" -H "X-Corelamo-Key: $ADMIN_TOKEN"


section "Get-logs"

check "get logs no database" 404 -X GET "$BASE_URL/api/databases/get-logs" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "get logs database empty" 404 -X GET "$BASE_URL/api/databases//get-logs" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "get logs nonexistent database" 404 -X GET "$BASE_URL/api/databases/yo/get-logs" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "get logs database one(running)" 200 -X GET "$BASE_URL/api/databases/$DB1/get-logs" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "get logs database two(stopped)" 200 -X GET "$BASE_URL/api/databases/$DB2/get-logs" -H "X-Corelamo-Key: $ADMIN_TOKEN"


section "Clear-logs"

check "clear logs no database" 404 -X DELETE "$BASE_URL/api/databases/clear-logs" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "clear logs database empty" 404 -X DELETE "$BASE_URL/api/databases//clear-logs" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "clear logs nonexistent database" 404 -X DELETE "$BASE_URL/api/databases/yo/clear-logs" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "clear logs database one(running)" 200 -X DELETE "$BASE_URL/api/databases/$DB1/clear-logs" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "clear logs database one again" 200 -X DELETE "$BASE_URL/api/databases/$DB1/clear-logs" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "clear logs database two(stopped)" 200 -X DELETE "$BASE_URL/api/databases/$DB2/clear-logs" -H "X-Corelamo-Key: $ADMIN_TOKEN"


section "List-databases"

check "list databases" 200 -X GET "$BASE_URL/api/list-databases" -H "X-Corelamo-Key: $ADMIN_TOKEN"
    check_json_bool "data should be 2" '(.data.databases | length) == 2'

section "Reindex"

check "reindex no db" 404 -X POST "$BASE_URL/api/databases/reindex" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "reindex db empty" 404 -X POST "$BASE_URL/api/databases//reindex" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "reindex nonexistent db" 404 -X POST "$BASE_URL/api/databases/yo/reindex" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "reindex one(running)" 200 -X POST "$BASE_URL/api/databases/$DB1/reindex" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "reindex two(stopped)" 409 -X POST "$BASE_URL/api/databases/$DB2/reindex" -H "X-Corelamo-Key: $ADMIN_TOKEN"


section "Policy"

check "get policy no database" 404 -X GET "$BASE_URL/api/databases/get-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "get policy empty database" 404 -X GET "$BASE_URL/api/databases//get-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "get policy nonexistent database" 404 -X GET "$BASE_URL/api/databases/yo/get-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "get policy one" 200 -X GET "$BASE_URL/api/databases/$DB1/get-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "get policy two" 200 -X GET "$BASE_URL/api/databases/$DB2/get-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "set policy no database" 404 -X POST "$BASE_URL/api/databases/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
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
check "set policy empty database" 404 -X POST "$BASE_URL/api/databases//set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
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
check "set policy nonexistent database" 404 -X POST "$BASE_URL/api/databases/yo/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
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
check "set empty policy" 400 -X POST "$BASE_URL/api/databases/$DB1/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d ''
check "set invalid toml policy" 400 -X POST "$BASE_URL/api/databases/$DB1/set-policy" -H "X-Corelamo-Key: $ACorelamoError::Internal(format!("failed to get stats: {e}")),DMIN_TOKEN" \
  -d '
  [[fields]]
  name = id
  xpath = 0
  index = Id
  list = true
  [fields.weight]
  min = 100
  max = 100

  [[fields]]
  name
  xpath = 1
  index = "Text"
  = true
  [fields.weight]
  min = 100
  max = 100
  '
check "set invalid policy" 400 -X POST "$BASE_URL/api/databases/$DB1/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '
  [[fields]]
  name = "id"
  xpath = 0
  index = "Yo"
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
check "set repeating id fields policy" 400 -X POST "$BASE_URL/api/databases/$DB1/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
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
  index = "Id"
  list = true
  [fields.weight]
  min = 100
  max = 100
  '
check "set repeating id fields policy" 400 -X POST "$BASE_URL/api/databases/$DB1/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
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
  name = "id"
  xpath = 1
  index = "Id"
  list = true
  [fields.weight]
  min = 100
  max = 100
  '
check "set repeating id fields policy same name same xpath" 400 -X POST "$BASE_URL/api/databases/$DB1/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
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
  name = "id"
  xpath = 0
  index = "Id"
  list = true
  [fields.weight]
  min = 100
  max = 100
  '
check "set repeating id fields policy diff name same xpath" 400 -X POST "$BASE_URL/api/databases/$DB1/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
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
  name = "title"
  xpath = 0
  index = "Id"
  list = true
  [fields.weight]
  min = 100
  max = 100
  '
check "set repeating title fields policy same xpath" 400 -X POST "$BASE_URL/api/databases/$DB1/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
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
  name = "title"
  xpath = 1
  index = "Text"
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
check "set repeating title fields policy diff xpath" 400 -X POST "$BASE_URL/api/databases/$DB1/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
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
  name = "title"
  xpath = 1
  index = "Text"
  list = true
  [fields.weight]
  min = 100
  max = 100
  
  [[fields]]
  name = "title"
  xpath = 2
  index = "Text"
  list = true
  [fields.weight]
  min = 100
  max = 100
  '
check "set missing weights policy" 400 -X POST "$BASE_URL/api/databases/$DB1/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '
  [[fields]]
  name = "id"
  xpath = 0
  index = "Id"
  list = true
  [fields.weight]

  [[fields]]
  name = "number"
  index = "Text"
  list = true
  [fields.weight]
  min = 100
  max = 100
  '
check "set policy one" 200 -X POST "$BASE_URL/api/databases/$DB1/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
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
check "set policy two" 200 -X POST "$BASE_URL/api/databases/$DB2/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
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
#

section "Config"

check "get config no database" 404 -X GET "$BASE_URL/api/databases/get-config" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "get config empty database" 404 -X GET "$BASE_URL/api/databases//get-config" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "get config nonexistent database" 404 -X GET "$BASE_URL/api/databases/yo/get-config" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "get config one" 200 -X GET "$BASE_URL/api/databases/$DB1/get-config" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "get config two" 200 -X GET "$BASE_URL/api/databases/$DB2/get-config" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "set empty config" 400 -X PUT "$BASE_URL/api/databases/$DB1/set-config" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d ''
check "set invalid toml config" 400 -X PUT "$BASE_URL/api/databases/$DB1/set-config" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d 'this shit invalid'
check "set invalid config" 400 -X PUT "$BASE_URL/api/databases/$DB1/set-config" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '
  enable_background_compaction = hujz
  bootable = nezinu

  [runtime]
  flush_threshold = 1000000000000000000000000000
  indexing_batch_size = yes
  indexing_window_size = 10yo000

  [runtime.compaction]
  max_segments_per_compaction = 8
  compact_when_segments_at_least = 16

  [compaction_interval]
  secs = 1
  nanos = 0
  '
check "set repeating fields config" 400 -X PUT "$BASE_URL/api/databases/$DB1/set-config" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '
  enable_background_compaction = true
  bootable = false
  bootable = true

  [runtime]
  flush_threshold = 100000
  indexing_batch_size = 100000
  indexing_window_size = 10000
  indexing_window_size = 1

  [runtime.compaction]
  max_segments_per_compaction = 8
  compact_when_segments_at_least = 16

  [compaction_interval]
  secs = 1
  nanos = 0
  '
check "set config missing field" 400 -X PUT "$BASE_URL/api/databases/$DB1/set-config" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '
  enable_background_compaction = true
  bootable = false

  [runtime]
  flush_threshold = 100000
  indexing_window_size = 10000

  [runtime.compaction]
  max_segments_per_compaction = 8
  compact_when_segments_at_least = 16

  [compaction_interval]
  secs = 1
  nanos = 0
  '
check "set config one" 200 -X PUT "$BASE_URL/api/databases/$DB1/set-config" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
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
check "set config two" 200 -X PUT "$BASE_URL/api/databases/$DB2/set-config" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
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
#

section "Delete-database"

check "delete no database" 404 -X DELETE "$BASE_URL/api/databases/delete-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "delete database empty" 404 -X DELETE "$BASE_URL/api/databases//delete-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "delete nonexistent database" 404 -X DELETE "$BASE_URL/api/databases/yo/delete-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "delete database one" 200 -X DELETE "$BASE_URL/api/databases/$DB1/delete-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "delete database two" 200 -X DELETE "$BASE_URL/api/databases/$DB2/delete-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"


echo
echo "=============================================================="
echo " Results: $PASS_COUNT passed, $FAIL_COUNT failed"
echo "=============================================================="

rm -f /tmp/search_test_body.json

if [ "$FAIL_COUNT" -gt 0 ]; then
    exit 1
fi