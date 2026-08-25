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
  index = "Id"
  list = true
  [fields.weight]
  min = 100
  max = 100

  [[fields]]
  name = "number"
  index = "Text"
  list = true
  [fields.weight]
  min = 100
  max = 100
  '

check "set config" 200 -X POST "$BASE_URL/api/databases/$DB/set-config" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '
  enable_background_compaction = true
  bootable = false
  shard_count = 5

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
  
  [incremental_backup_interval]
  secs = 3600
  nanos = 0

  [full_backup_interval]
  secs = 86400
  nanos = 0

  [backup_lifetime]
  secs = 604800
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
  check "insert bad doc" 400 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d 'jaja'

check "insert normal document" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"1", "number":"1"}'

check "insert duplicate id" 409 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"1", "number":"none"}'

check "insert text id" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"bruh", "number":"2"}'

check "insert duplicate text id" 409 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"bruh", "number":"none"}'

check "insert document with two ids" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"no","id":"way", "number":"3"}'
check "retrieve documents" 200 -X POST "$BASE_URL/api/databases/$DB/retrieve" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '["no","way"]'
#neatrod no, atrod way(dokuments tikai ar id=way,number=none)

check "insert document with id 3" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"3", "number":"4"}'


section "Insert with autoincrement id"

check "set autoid policy" 200 -X POST "$BASE_URL/api/databases/$DB/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '
  [[fields]]
  name = "id"
  index = "IdAuto"
  list = true
  [fields.weight]
  min = 100
  max = 100

  [[fields]]
  name = "number"
  index = "Text"
  list = true
  [fields.weight]
  min = 100
  max = 100
  '


check "insert document with id value" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"4", "number":"5"}'
check "insert document with duplicate id value" 409 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"4", "number":"5"}'
check "insert document with id text value" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"yo", "number":"6"}'
check "insert document without id value" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"", "number":"Latvija"}'
check "insert document without id" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"number":"Latvija"}'


# ------------------------------------------------------------------
# Replace
# ------------------------------------------------------------------

section "Replace"

check "replace bad document" 400 -X POST "$BASE_URL/api/databases/$DB/replace" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d 'bad'
check "replace non existing id" 404 -X POST "$BASE_URL/api/databases/$DB/replace" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"idk","number":"none"}'
#sitie saka not_found nevis ka trukst id value
check "replace empty id" 404 -X POST "$BASE_URL/api/databases/$DB/replace" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"","number":"none"}'
check "replace no id" 404 -X POST "$BASE_URL/api/databases/$DB/replace" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"number":"none"}'

#vins kroc replaco to dokumentu kura id ir pedejaa id field
check "replace two ids" 200 -X POST "$BASE_URL/api/databases/$DB/replace" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"1","id":"3","number":"three"}'
check "retrieve check both" 200 -X POST "$BASE_URL/api/databases/$DB/retrieve" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '["1","3"]'

check "replace existing number id" 200 -X POST "$BASE_URL/api/databases/$DB/replace" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"1","number":"one"}'
check "replace existing text id" 200 -X POST "$BASE_URL/api/databases/$DB/replace" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"bruh","number":"two"}'



# ------------------------------------------------------------------
# Upsert
# ------------------------------------------------------------------

section "Upsert"

check "upsert a non existing id" 200 -X POST "$BASE_URL/api/databases/$DB/upsert" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"kkads aidi","number":"9"}'
check "upsert empty id auto" 200 -X POST "$BASE_URL/api/databases/$DB/upsert" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"","number":"9/10"}'
check "upsert bad doc" 400 -X POST "$BASE_URL/api/databases/$DB/upsert" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d 'slikts'

check "upsert missing id auto" 200 -X POST "$BASE_URL/api/databases/$DB/upsert" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"number":"10/11"}'


check "set no autoid policy" 200 -X POST "$BASE_URL/api/databases/$DB/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '
  [[fields]]
  name = "id"
  index = "Id"
  list = true
  [fields.weight]
  min = 100
  max = 100

  [[fields]]
  name = "number"
  index = "Text"
  list = true
  [fields.weight]
  min = 100
  max = 100
  '
check "upsert empty id" 409 -X POST "$BASE_URL/api/databases/$DB/upsert" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"","number":"naw"}'
check "upsert missing id" 409 -X POST "$BASE_URL/api/databases/$DB/upsert" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"number":"none"}'

check "upsert an existing id" 200 -X POST "$BASE_URL/api/databases/$DB/upsert" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"4","number":"four"}'
check "upsert a new id" 200 -X POST "$BASE_URL/api/databases/$DB/upsert" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"id":"what","number":"yo"}'

# ------------------------------------------------------------------
# Retrieve
# ------------------------------------------------------------------

section "Retrieve"

check "retrieve nothing" 200 -X POST "$BASE_URL/api/databases/$DB/retrieve" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '[]'
check "retrieve no doc" 400 -X POST "$BASE_URL/api/databases/$DB/retrieve" -H "X-Corelamo-Key: $ADMIN_TOKEN" 
check "retrieve bad doc" 400 -X POST "$BASE_URL/api/databases/$DB/retrieve" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d 'nezkkas'

check "retrieve non existing" 200 -X POST "$BASE_URL/api/databases/$DB/retrieve" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '[""]'
check "retrieve multiple non existing" 200 -X POST "$BASE_URL/api/databases/$DB/retrieve" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '["","yow"]'

check "retrieve one real" 200 -X POST "$BASE_URL/api/databases/$DB/retrieve" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '["7"]'
check "retrieve text id" 200 -X POST "$BASE_URL/api/databases/$DB/retrieve" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '["bruh"]'
check "retrieve autoincremented" 200 -X POST "$BASE_URL/api/databases/$DB/retrieve" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '["13"]'
check "retrieve multiple real" 200 -X POST "$BASE_URL/api/databases/$DB/retrieve" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '["1","2","3"]'
check "retrieve mixed real notreal" 200 -X POST "$BASE_URL/api/databases/$DB/retrieve" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '["7", "100", "1", "5", "bruh", "0"]'

# ------------------------------------------------------------------
# Lookup
# ------------------------------------------------------------------

section "Lookup"

check "lookup nothing" 200 -X POST "$BASE_URL/api/databases/$DB/lookup" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"ids":[""],"return_fields": {"number":true}}'
check "lookup no doc" 400 -X POST "$BASE_URL/api/databases/$DB/lookup" -H "X-Corelamo-Key: $ADMIN_TOKEN" 
check "lookup bad doc" 400 -X POST "$BASE_URL/api/databases/$DB/lookup" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d 'nezkkas'

check "lookup non existing" 200 -X POST "$BASE_URL/api/databases/$DB/lookup" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"ids":["nenenenene"]}'
check "lookup multiple non existing" 200 -X POST "$BASE_URL/api/databases/$DB/lookup" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"ids":["nenenenene","jajajajjaj"]}'
#  -d '{"ids":["nenenenene","jajajajjaj"],"return_fields": {"id":false,"number":true}}'

check "lookup one real" 200 -X POST "$BASE_URL/api/databases/$DB/lookup" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"ids":["7"],"return_fields": {"id":false,"number":true}}'
check "lookup text id" 200 -X POST "$BASE_URL/api/databases/$DB/lookup" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"ids":["bruh"]}'
#check "lookup autoincremented" 200 -X POST "$BASE_URL/api/databases/$DB/lookup" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#  -d '{"ids":["13"]}'
check "lookup multiple real" 200 -X POST "$BASE_URL/api/databases/$DB/lookup" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"ids":["1","3","4"]}'
check "lookup multiple real" 200 -X POST "$BASE_URL/api/databases/$DB/lookup" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"ids":["1","1"]}'
check "lookup mixed real notreal" 200 -X POST "$BASE_URL/api/databases/$DB/lookup" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"ids":["7","100","1","5","bruh","0"]}'

# ------------------------------------------------------------------
# Partial replace
# ------------------------------------------------------------------

section "Partial replace"

check "seed partial replace" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"p1", "something":"yo", "otherthing":true, "what":{"is":"this","idk":7}}'
check "seed partial replace" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"p2", "something":"yo", "otherthing":true, "what":{"is":"this","idk":7}}'

check "partial-replace nothing" 400 -X POST "$BASE_URL/api/databases/$DB/partial-replace" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d ''
check "partial-replace bad doc" 400 -X POST "$BASE_URL/api/databases/$DB/partial-replace" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '{"jabut":"array"}'
check "partial-replace no patch" 400 -X POST "$BASE_URL/api/databases/$DB/partial-replace" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '[{"id":"p1"]'
check "partial-replace nonexisting" 404 -X POST "$BASE_URL/api/databases/$DB/partial-replace" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '[{"id":"nevarbut","patch":{"kkas":"kkas"}}]'

check "partial-replace two" 200 -X POST "$BASE_URL/api/databases/$DB/partial-replace" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '[
        {
          "id": "p1",
          "patch":
            {
              "something" : "wazzap",
              "otherthing":
                {
                  "now":"this is object"
                },
              "newfield":"hello"
            }
        },
        {
          "id": "p2",
          "patch" :
            {
              "what":
              {
                "idk":"now text"
              }
            }
        }
      ]'

check "partial-replace empty patch" 200 -X POST "$BASE_URL/api/databases/$DB/partial-replace" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '[{"id": "p1","patch":{}}]'
#nekas nemainaas

check "partial-replace one" 200 -X POST "$BASE_URL/api/databases/$DB/partial-replace" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '[{"id":"p2","patch":{"something":"no"}}]'

check "partial-replace id" 200 -X POST "$BASE_URL/api/databases/$DB/partial-replace" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '[{"id":"p2","patch":{"id":"p3"}}]'
check "partial-replace remove field" 200 -X POST "$BASE_URL/api/databases/$DB/partial-replace" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '[{"id":"p1","patch":{"newfield":null}}]'
check "partial-replace remove nonexistent field" 200 -X POST "$BASE_URL/api/databases/$DB/partial-replace" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '[{"id":"p1","patch":{"frield":null}}]'
check "partial-replace remove object field" 200 -X POST "$BASE_URL/api/databases/$DB/partial-replace" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '[{"id":"p1","patch":{"what":null}}]'
check "partial-replace remove id" 200 -X POST "$BASE_URL/api/databases/$DB/partial-replace" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '[{"id":"p2","patch":{"id":null}}]'




# ------------------------------------------------------------------
# All fields
# ------------------------------------------------------------------

#section "All fields"

#curl -X GET "$BASE_URL/api/databases/$DB/all-fields" -H "X-Corelamo-Key: $ADMIN_TOKEN"

#check "insert remove1" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#  -d '{"remove1":"yo", "id":"a1"}'
#check "insert remove2" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#  -d '{"remove2":null, "id":"a2"}'
#check "insert remove3" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#  -d '{"remove3":"", "id":"a3"}'

#curl -X POST "$BASE_URL/api/databases/$DB/retrieve" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#  -d '["a1","a2","a3"]'

#curl -X GET "$BASE_URL/api/databases/$DB/all-fields" -H "X-Corelamo-Key: $ADMIN_TOKEN"

#check "remove1" 200 -X POST "$BASE_URL/api/databases/$DB/partial-replace" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '[{"id":"a1","patch":{"remove1":null}}]'
#check "remove2" 200 -X POST "$BASE_URL/api/databases/$DB/partial-replace" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '[{"id":"a2","patch":{"remove2":null}}]'
#check "remove3" 200 -X POST "$BASE_URL/api/databases/$DB/partial-replace" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '[{"id":"a3","patch":{"remove3":null}}]'

#check "delete existing id" 200 -X DELETE "$BASE_URL/api/databases/$DB/delete" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '["a1","a2"]'

#curl -X POST "$BASE_URL/api/databases/$DB/reindex" -H "X-Corelamo-Key: $ADMIN_TOKEN"

#sleep 5

#curl -X POST "$BASE_URL/api/databases/$DB/retrieve" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#  -d '["a1","a2","a3"]'

#curl -X GET "$BASE_URL/api/databases/$DB/all-fields" -H "X-Corelamo-Key: $ADMIN_TOKEN"

# ------------------------------------------------------------------
# Delete
# ------------------------------------------------------------------

section "Delete"

check "delete bad document" 400 -X DELETE "$BASE_URL/api/databases/$DB/delete" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d 'delit'
check "delete existing id" 200 -X DELETE "$BASE_URL/api/databases/$DB/delete" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '["1"]'
check "delete multiple existing id" 200 -X DELETE "$BASE_URL/api/databases/$DB/delete" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '["3","4"]'
check "delete existing text id" 200 -X DELETE "$BASE_URL/api/databases/$DB/delete" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '["bruh"]'

check "delete non existing" 404 -X DELETE "$BASE_URL/api/databases/$DB/delete" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '["neeksiste"]'
check "delete non existing, plus existing" 207 -X DELETE "$BASE_URL/api/databases/$DB/delete" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '["neeksiste","yo"]'
check "delete multiple non existinging" 404 -X DELETE "$BASE_URL/api/databases/$DB/delete" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '["neeksiste","neeksiste2"]'
check "delete no document" 400 -X DELETE "$BASE_URL/api/databases/$DB/delete" -H "X-Corelamo-Key: $ADMIN_TOKEN"

check "delete empty id" 404 -X DELETE "$BASE_URL/api/databases/$DB/delete" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '[""]'
#vins uzskata "" ka id un so id neatrod, tapec 404

check "delete no id" 200 -X DELETE "$BASE_URL/api/databases/$DB/delete" -H "X-Corelamo-Key: $ADMIN_TOKEN" -d '[]'
#vins mekle 0 ids un atrod 0 tapec viss ok

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