#!/bin/bash
# ==============================================================================
# Creates a Corelamo database and inserts power-set documents for the
# vocabulary: alpha beta gamma delta epsilon zeta eta theta (n=8)
#
# One document per non-empty subset of the vocabulary -> 2^8 - 1 = 255 docs.
# Title = subset words joined by spaces (in vocabulary order).
#
# HOW TO USE:
#   1. Start the server first:  cargo run -p core-runtime -- --root-path /tmp
#   2. Run this file:           bash dataset.sh
# ==============================================================================
clear

BASE_URL="http://localhost:6006"
DB="docs"
VOCAB=(alpha beta gamma delta epsilon zeta eta theta)

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
curl -s -X DELETE $BASE_URL/api/databases/$DB/delete-database -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "create database" 201 -X POST "$BASE_URL/api/databases/$DB/create-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "start database" 200 -X POST "$BASE_URL/api/databases/$DB/start-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
check "set policy" 200 -X POST "$BASE_URL/api/databases/$DB/set-policy" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '
  [[fields]]
  name = "id"
  xpath = 0
  index = "IdAutoIncrement"
  list = false
  [fields.weight]
  min = 100
  max = 100

  [[fields]]
  name = "title"
  xpath = 1
  index = "Text"
  list = false
  [fields.weight]
  min = 100
  max = 100
  '

check "set config" 200 -X PUT "$BASE_URL/api/databases/$DB/set-config" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
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

check "insert first document" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
        -d '{"title":"EXAMPLE DATABASE DOCUMENT"}'
# ------------------------------------------------------------------
# Power-set document generation & insertion
# ------------------------------------------------------------------
# Iterate every non-empty subset of VOCAB using a bitmask over its indices.
# For n=8, bitmask ranges 1..255 (0 = empty set, skipped).

N=${#VOCAB[@]}
TOTAL=$(( (1 << N) - 1 ))

echo
echo "Inserting $TOTAL documents..."
 
for (( mask=1; mask<=TOTAL; mask++ )); do
    title=""
    for (( bit=0; bit<N; bit++ )); do
        if (( (mask >> bit) & 1 )); then
            if [ -z "$title" ]; then
                title="${VOCAB[$bit]}"
            else
                title="$title ${VOCAB[$bit]}"
            fi
        fi
    done
 
    check "insert doc (mask=$mask)" 200 -X POST "$BASE_URL/api/databases/$DB/insert" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
        -d "{\"title\":\"$title\"}"
done
 
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
 