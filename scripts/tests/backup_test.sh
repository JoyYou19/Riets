#!/bin/bash
# ==============================================================================
# Corelamo — reindex / backup / restore concurrency matrix  (auth disabled)
#
# Focus: what happens when the three long-running operations overlap.
#   Each pair is started ~simultaneously, then the script verifies:
#     - both requests returned a sane status (no hang, no 500)
#     - operations actually finished (nothing stuck in "reindexing")
#     - the database is still usable and still holds its documents
#
# USAGE:
#   1. start the server (auth off)
#   2. bash concurrency_test.sh
# ==============================================================================
clear

BASE_URL="http://localhost:6006"
DB="conc_$(date +%s)"
SEED_DOCS=50000         # big enough that operations take measurable time
POLL_TIMEOUT=180        # max seconds to wait for an operation to settle

PASS=0
FAIL=0

# ------------------------------------------------------------------
# helpers
# ------------------------------------------------------------------

section() {
    echo
    echo "=============================================================="
    echo " $1"
    echo "=============================================================="
}

pass() { echo "PASS  [$1] $2"; PASS=$((PASS + 1)); }
fail() { echo "FAIL  [$1] $2"; FAIL=$((FAIL + 1)); }

# check <label> <expected-codes "200|409"> <curl args...>
check() {
    local label="$1" expected="$2"; shift 2
    local code
    code=$(curl -s -o /tmp/conc_body.json -w "%{http_code}" "$@")
    if [[ "|$expected|" == *"|$code|"* ]]; then
        pass "$label" "got $code"
    else
        fail "$label" "expected $expected, got $code"
        echo "      body: $(head -c 400 /tmp/conc_body.json)"
    fi
}

# fire <outfile> <curl args...>   — background request, status code lands in outfile
fire() {
    local out="$1"; shift
    (
        code=$(curl -s -o "${out}.body" -w "%{http_code}" "$@")
        echo "$code" > "$out"
    ) &
}

status_json()    { curl -s -X GET "$BASE_URL/api/databases/$DB/status"; }
doc_count()      { status_json | jq -r '.data.document_count // "ERR"'; }
reindex_status() { status_json | jq -r '.data.reindexing.status // "ERR"'; }
reindex_pct()    { status_json | jq -r '.data.reindexing.progress // 0'; }
backup_count()   { curl -s -X GET "$BASE_URL/api/databases/$DB/list-backups" \
                     | jq -r '(.data.backups | length) // 0'; }

latest_full_backup_id() {
    curl -s -X GET "$BASE_URL/api/databases/$DB/list-backups" \
        | jq -r '.data.backups | map(select(.backup_type == "Full"))
                 | sort_by(.created_at) | last | .backup_id // empty'
}

# blocks until reindex leaves the "reindexing" state, or times out
wait_for_reindex() {
    local label="$1"
    local waited=0
    while [ "$(reindex_status)" == "reindexing" ]; do
        sleep 0.5
        waited=$((waited + 1))
        if [ $((waited / 2)) -ge $POLL_TIMEOUT ]; then
            fail "$label" "reindex stuck at $(reindex_pct)% after ${POLL_TIMEOUT}s"
            return 1
        fi
    done
    return 0
}

# the database must still answer and still hold its documents
assert_intact() {
    local label="$1" expected_docs="$2"
    local actual
    actual=$(doc_count)

    if [ "$actual" == "$expected_docs" ]; then
        pass "$label" "document_count intact ($actual)"
    else
        fail "$label" "document_count expected $expected_docs, got $actual"
    fi

    local code
    code=$(curl -s -o /tmp/conc_body.json -w "%{http_code}" \
        -X POST "$BASE_URL/api/databases/$DB/search" -d '{"query":"alpha","docs":5}')
    if [ "$code" == "200" ]; then
        local hits
        hits=$(jq -r '(.data | if type=="array" then length else (.hits | length) end) // 0' \
               /tmp/conc_body.json 2>/dev/null)
        if [ "${hits:-0}" -gt 0 ] 2>/dev/null; then
            pass "$label" "search returned $hits hits"
        else
            fail "$label" "search returned 200 but no hits — index may be empty"
        fi
    else
        fail "$label" "search failed with $code"
    fi
}

# both codes of a concurrent pair must land in the allowed set
assert_pair() {
    local label="$1" allowed="$2" a="$3" b="$4" name_a="$5" name_b="$6"
    echo "  $name_a: $a    $name_b: $b"
    [[ "|$allowed|" == *"|$a|"* ]] \
        && pass "$label / $name_a" "got $a" \
        || fail "$label / $name_a" "expected one of $allowed, got $a"
    [[ "|$allowed|" == *"|$b|"* ]] \
        && pass "$label / $name_b" "got $b" \
        || fail "$label / $name_b" "expected one of $allowed, got $b"
}

echo "=========================================="
echo " Corelamo concurrency matrix"
echo " database: $DB"
echo "=========================================="

# ------------------------------------------------------------------
section "SETUP"
# ------------------------------------------------------------------

check "create database" "201" -X POST "$BASE_URL/api/databases/$DB/create-database"
check "start database"  "200" -X POST "$BASE_URL/api/databases/$DB/start-database"

check "set policy" "200" -X POST "$BASE_URL/api/databases/$DB/set-policy" -d '
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
'

echo "Seeding $SEED_DOCS documents..."
payload="["
for (( i=1; i<=SEED_DOCS; i++ )); do
    [ $i -gt 1 ] && payload="$payload,"
    payload="$payload{\"title\":\"alpha beta document number $i\"}"
done
payload="$payload]"
check "seed documents" "200" -X POST "$BASE_URL/api/databases/$DB/insert" -d "$payload"

BASELINE=$(doc_count)
echo "Baseline document_count: $BASELINE"

if [ "$BASELINE" == "ERR" ] || [ "$BASELINE" == "0" ] || [ -z "$BASELINE" ]; then
    echo
    echo "ABORT: could not read a document count. Raw status response:"
    status_json | head -c 1200
    echo
    exit 1
fi

# ------------------------------------------------------------------
section "BASELINE — each operation alone"
# ------------------------------------------------------------------

check "reindex alone" "200" -X POST "$BASE_URL/api/databases/$DB/reindex"
wait_for_reindex "reindex alone"
assert_intact "after solo reindex" "$BASELINE"

check "backup alone" "200" -X POST "$BASE_URL/api/databases/$DB/backup"
sleep 3
echo "  backups on disk: $(backup_count)"
assert_intact "after solo backup" "$BASELINE"

BACKUP_ID=$(latest_full_backup_id)
echo "  restore target: ${BACKUP_ID:-<none found>}"

if [ -z "$BACKUP_ID" ]; then
    echo "WARN: no full backup id found — restore pairs will be skipped."
else
    check "restore alone" "200" -X POST "$BASE_URL/api/databases/$DB/restore-backup/$BACKUP_ID"
    sleep 4
    assert_intact "after solo restore" "$BASELINE"
fi

# ==================================================================
section "PAIR 1 — reindex + backup"
# ==================================================================

fire /tmp/c_p1a -X POST "$BASE_URL/api/databases/$DB/reindex"
sleep 0.1
fire /tmp/c_p1b -X POST "$BASE_URL/api/databases/$DB/backup"
wait

assert_pair "reindex+backup" "200|409|503" \
    "$(cat /tmp/c_p1a)" "$(cat /tmp/c_p1b)" "reindex" "backup"

wait_for_reindex "reindex+backup"
sleep 3
assert_intact "after reindex+backup" "$BASELINE"

# ==================================================================
section "PAIR 2 — backup + reindex (reverse order)"
# ==================================================================

fire /tmp/c_p2a -X POST "$BASE_URL/api/databases/$DB/backup"
sleep 0.1
fire /tmp/c_p2b -X POST "$BASE_URL/api/databases/$DB/reindex"
wait

assert_pair "backup+reindex" "200|409|503" \
    "$(cat /tmp/c_p2a)" "$(cat /tmp/c_p2b)" "backup" "reindex"

wait_for_reindex "backup+reindex"
sleep 3
assert_intact "after backup+reindex" "$BASELINE"

# ==================================================================
section "PAIR 3 — reindex + reindex (duplicate must be rejected)"
# ==================================================================

fire /tmp/c_p3a -X POST "$BASE_URL/api/databases/$DB/reindex"
sleep 0.1
fire /tmp/c_p3b -X POST "$BASE_URL/api/databases/$DB/reindex"
wait

A=$(cat /tmp/c_p3a); B=$(cat /tmp/c_p3b)
echo "  first: $A    second: $B"
if [ "$A" == "200" ] && [[ "$B" =~ ^(409|503)$ ]]; then
    pass "reindex+reindex" "second correctly rejected ($B)"
else
    fail "reindex+reindex" "expected 200 then 409/503, got $A / $B"
fi

wait_for_reindex "reindex+reindex"
assert_intact "after reindex+reindex" "$BASELINE"

# ==================================================================
section "PAIR 4 — backup + backup"
# ==================================================================

BEFORE_BACKUPS=$(backup_count)

fire /tmp/c_p4a -X POST "$BASE_URL/api/databases/$DB/backup"
sleep 0.1
fire /tmp/c_p4b -X POST "$BASE_URL/api/databases/$DB/backup"
wait

assert_pair "backup+backup" "200|409|503" \
    "$(cat /tmp/c_p4a)" "$(cat /tmp/c_p4b)" "backup-1" "backup-2"

sleep 3
AFTER_BACKUPS=$(backup_count)
echo "  backups before: $BEFORE_BACKUPS   after: $AFTER_BACKUPS"
if [ "$AFTER_BACKUPS" -gt "$BEFORE_BACKUPS" ]; then
    pass "backup+backup" "at least one backup was written"
else
    fail "backup+backup" "no new backup appeared"
fi
assert_intact "after backup+backup" "$BASELINE"

# ==================================================================
section "PAIR 5 — restore + reindex"
# ==================================================================

if [ -n "$BACKUP_ID" ]; then
    fire /tmp/c_p5a -X POST "$BASE_URL/api/databases/$DB/restore-backup/$BACKUP_ID"
    sleep 0.1
    fire /tmp/c_p5b -X POST "$BASE_URL/api/databases/$DB/reindex"
    wait

    assert_pair "restore+reindex" "200|409|503" \
        "$(cat /tmp/c_p5a)" "$(cat /tmp/c_p5b)" "restore" "reindex"

    wait_for_reindex "restore+reindex"
    sleep 4
    assert_intact "after restore+reindex" "$BASELINE"
else
    echo "SKIP — no backup id"
fi

# ==================================================================
section "PAIR 6 — reindex + restore (reverse order)"
# ==================================================================

if [ -n "$BACKUP_ID" ]; then
    fire /tmp/c_p6a -X POST "$BASE_URL/api/databases/$DB/reindex"
    sleep 0.1
    fire /tmp/c_p6b -X POST "$BASE_URL/api/databases/$DB/restore-backup/$BACKUP_ID"
    wait

    assert_pair "reindex+restore" "200|409|503" \
        "$(cat /tmp/c_p6a)" "$(cat /tmp/c_p6b)" "reindex" "restore"

    wait_for_reindex "reindex+restore"
    sleep 4
    assert_intact "after reindex+restore" "$BASELINE"
else
    echo "SKIP — no backup id"
fi

# ==================================================================
section "PAIR 7 — restore + backup"
# ==================================================================

if [ -n "$BACKUP_ID" ]; then
    fire /tmp/c_p7a -X POST "$BASE_URL/api/databases/$DB/restore-backup/$BACKUP_ID"
    sleep 0.1
    fire /tmp/c_p7b -X POST "$BASE_URL/api/databases/$DB/backup"
    wait

    assert_pair "restore+backup" "200|409|503" \
        "$(cat /tmp/c_p7a)" "$(cat /tmp/c_p7b)" "restore" "backup"

    sleep 4
    assert_intact "after restore+backup" "$BASELINE"
else
    echo "SKIP — no backup id"
fi

# ==================================================================
section "PAIR 8 — restore + restore"
# ==================================================================

if [ -n "$BACKUP_ID" ]; then
    fire /tmp/c_p8a -X POST "$BASE_URL/api/databases/$DB/restore-backup/$BACKUP_ID"
    sleep 0.1
    fire /tmp/c_p8b -X POST "$BASE_URL/api/databases/$DB/restore-backup/$BACKUP_ID"
    wait

    assert_pair "restore+restore" "200|409|503" \
        "$(cat /tmp/c_p8a)" "$(cat /tmp/c_p8b)" "restore-1" "restore-2"

    sleep 4
    assert_intact "after restore+restore" "$BASELINE"
else
    echo "SKIP — no backup id"
fi

# ==================================================================
section "TRIPLE — reindex + backup + restore all at once"
# ==================================================================

if [ -n "$BACKUP_ID" ]; then
    fire /tmp/c_t1 -X POST "$BASE_URL/api/databases/$DB/reindex"
    fire /tmp/c_t2 -X POST "$BASE_URL/api/databases/$DB/backup"
    fire /tmp/c_t3 -X POST "$BASE_URL/api/databases/$DB/restore-backup/$BACKUP_ID"
    wait

    echo "  reindex: $(cat /tmp/c_t1)   backup: $(cat /tmp/c_t2)   restore: $(cat /tmp/c_t3)"
    for f in /tmp/c_t1 /tmp/c_t2 /tmp/c_t3; do
        code=$(cat $f)
        [[ "$code" =~ ^(200|409|503)$ ]] \
            && pass "triple" "$(basename $f) returned $code" \
            || fail "triple" "$(basename $f) returned $code (expected 200/409/503)"
    done

    wait_for_reindex "triple"
    sleep 5
    assert_intact "after triple overlap" "$BASELINE"
else
    echo "SKIP — no backup id"
fi

# ==================================================================
section "AVAILABILITY — normal traffic while a reindex runs"
# ==================================================================

check "kick off reindex" "200|409|503" -X POST "$BASE_URL/api/databases/$DB/reindex"

for i in 1 2 3 4 5; do
    check "search during reindex ($i)" "200" \
        -X POST "$BASE_URL/api/databases/$DB/search" -d '{"query":"alpha","docs":3}'
    check "status during reindex ($i)" "200" \
        -X GET "$BASE_URL/api/databases/$DB/status"
    echo "    progress: $(reindex_pct)%   docs: $(doc_count)"
    sleep 0.4
done

wait_for_reindex "availability"
assert_intact "after availability run" "$BASELINE"

# ==================================================================
section "CLEANUP — delete must succeed after all of the above"
# ==================================================================

sleep 2
check "delete database" "200" -X DELETE "$BASE_URL/api/databases/$DB/delete-database"

# ------------------------------------------------------------------
section "RESULTS"
# ------------------------------------------------------------------

echo " Passed: $PASS    Failed: $FAIL"
echo "=============================================================="

rm -f /tmp/conc_*

[ "$FAIL" -gt 0 ] && exit 1
exit 0