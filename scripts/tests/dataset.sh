#!/bin/bash
# ==============================================================================
# Creates a Corelamo database and inserts power-set documents for the
# vocabulary: alpha beta gamma delta epsilon zeta eta theta (n=8)
#
# One document per non-empty subset of the vocabulary -> 2^8 - 1 = 255 docs.
# Plus an extra document inserted first.
# Title = subset words joined by spaces (in vocabulary order).
#
# Every document also gets a nested "info" object:
#   info.number = the subset's bitmask (1-255)          -> unique per doc
#   info.date   = 2024-01-01 + bitmask days              -> mirrors number range-wise
#   info.text   = VOCAB2[popcount(bitmask) - 1]           -> depends only on word COUNT
#
# This makes every field independently predictable:
#   - number/date range queries map directly to mask ranges
#   - info.text:"four" matches exactly C(8,4) = 70 docs (all 4-word titles)
#   - combinations (e.g. title:alpha AND info.text:"one") stay exactly computable
#
# HOW TO USE:
#   1. Start the server first:  cargo run -p core-runtime -- --root-path /tmp
#   2. Run this file:           bash dataset.sh
# ==============================================================================
clear

BASE_URL="http://localhost:6006"
DB="docs"
VOCAB=(alpha beta gamma delta epsilon zeta eta theta)
VOCAB2=(one two three four five six seven eight)
ORDINAL=(first second third fourth fifth sixth seventh eighth)
BASE_DATE="2024-01-01"

PASS_COUNT=0
FAIL_COUNT=0

check() {
    local label="$1"
    local expected="$2"
    shift 2
    local response
    response=$(curl -s -o /tmp/powerset_body.json -w "%{http_code}" "$@")

    if [ "$response" == "$expected" ]; then
        PASS_COUNT=$((PASS_COUNT + 1))
    else
        echo "FAIL  [$label] expected $expected, got $response"
        echo "      body: $(cat /tmp/powerset_body.json)"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
}

get_token() {
    curl -s -X POST "$BASE_URL/api/login" -d "$1" | jq -r '.data.token'
}

ADMIN_TOKEN=$(get_token '{"username":"admin","password":"secret"}')

echo "=========================================="
echo " Corelamo dataset generator"
echo "=========================================="

# ------------------------------------------------------------------
# Database setup
# ------------------------------------------------------------------
curl -s -X GET $BASE_URL/api/databases/$DB/status -H "X-Corelamo-Key: $ADMIN_TOKEN"

curl -s -X DELETE $BASE_URL/api/databases/$DB/delete-database -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "create database" 201 -X POST "$BASE_URL/api/databases/$DB/create-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "start database" 200 -X POST "$BASE_URL/api/databases/$DB/start-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "set policy" 200 -X POST "$BASE_URL/api/databases/$DB/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '
  [[fields]]
  name = "id"
  index = "IdAuto"
  list = true
  [fields.weight]
  min = 100
  max = 100

  [[fields]]
  name = "title"
  index = "Text"
  list = true
  [fields.weight]
  min = 100
  max = 100

  [[fields]]
  name = "info/date"
  index = "Date"
  list = true
  [fields.weight]
  min = 100
  max = 100

  [[fields]]
  name = "info/number"
  index = "Number"
  list = true
  [fields.weight]
  min = 100
  max = 100

  [[fields]]
  name = "info/text"
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

  [backup_interval]
  secs = 3600
  nanos = 0
  '

check "insert first document" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
        -d '{"id":"1","title":"EXAMPLE DATABASE DOCUMENT", "info":{"date":"2024-01-01","number":0,"text":"zero"}}'

# ------------------------------------------------------------------
# Power-set document generation & insertion
# ------------------------------------------------------------------
# Iterate every non-empty subset of VOCAB using a bitmask over its indices.
# For n=8, bitmask ranges 1..255 (0 = empty set, skipped).

N=${#VOCAB[@]}
TOTAL=$(( (2 ** N) - 1 ))

echo
echo "Inserting $TOTAL documents..."

for (( mask=1; mask<=TOTAL; mask++ )); do
    title=""
    bit_count=0
    single_bit=-1
    for (( bit=0; bit<N; bit++ )); do
        if (( (mask >> bit) & 1 )); then
            if [ -z "$title" ]; then
                title="${VOCAB[$bit]}"
            else
                title="$title ${VOCAB[$bit]}"
            fi
            bit_count=$((bit_count + 1))
            single_bit=$bit
        fi
    done

    # info.number = mask itself (unique per doc, 1-255)
    number=$mask
    # info.date = base date + mask days (mirrors number range-wise)
    doc_date=$(date -d "$BASE_DATE + $mask days" +%Y-%m-%d)
    # info.text = word count spelled out (depends only on how many words the title has).
    # Single-word titles additionally get the ordinal word for that specific vocab word,
    # e.g. title "alpha" -> info.text "one first".
    if [ "$bit_count" -eq 1 ]; then
        text="${VOCAB2[$((bit_count - 1))]} ${ORDINAL[$single_bit]}"
    else
        text="${VOCAB2[$((bit_count - 1))]}"
    fi

    payload="{\"title\":\"$title\",\"info\":{\"date\":\"$doc_date\",\"number\":$number,\"text\":\"$text\"}}"

    check "insert doc (mask=$mask)" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
        -d "$payload"
done


check "insert deletable1" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"del1"}'
check "insert deletable2" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"id":"del2"}'

#check "insert 1" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#  -d '{"id":"0","title":"this id is a number"}'
#check "insert two" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#  -d '{"id":"two","title":"this id is text"}'
#check "insert auto" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#  -d '{"title":"this id is automatically generated"}'

#check "upsert auto" 200 -X POST "$BASE_URL/api/databases/$DB/upsert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#  -d '{"title": "upsert without id"}'
#check "upsert insert" 200 -X POST "$BASE_URL/api/databases/$DB/upsert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#  -d '{"id": "up", "title": "upsert with id"}'
#check "upsert update" 200 -X POST "$BASE_URL/api/databases/$DB/upsert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#  -d '{"id": "up", "title": "upsert updated"}'

#-------------------------------------------------

#curl -X POST $BASE_URL/api/databases/$DB/retrieve -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#    -d '["0", "two"]'
#curl -X DELETE $BASE_URL/api/databases/$DB/delete -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#    -d '["del1","del2"]'
#curl -X POST $BASE_URL/api/databases/$DB/lookup -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#    -d '{"ids": ["1"]}'
#curl -X POST $BASE_URL/api/databases/$DB/lookup -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#    -d '{"ids": ["1"],"return_fields": {"title":true,"info/text":false}}'

check "reindex" 200 -X POST "$BASE_URL/api/databases/$DB/reindex" -H "X-Corelamo-Key: $ADMIN_TOKEN"

echo
echo "=============================================================="
echo " Results: $PASS_COUNT passed, $FAIL_COUNT failed"
echo " Database '$DB' now has $TOTAL power-set documents (n=$N)."
echo " NOTE: database left running for querying/testing."
echo "       delete with: curl -X DELETE $BASE_URL/api/databases/$DB/delete-database -H \"X-Corelamo-Key: \$ADMIN_TOKEN\""
echo "=============================================================="

rm -f /tmp/powerset_body.json

if [ "$FAIL_COUNT" -gt 0 ]; then
    exit 1
fi