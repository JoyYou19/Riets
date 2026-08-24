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
#un ari visi search options kas sobrid nestrada

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

DB="docs"
check "start database" 200 -X POST "$BASE_URL/api/databases/$DB/start-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"

DOCUMENTS=$(curl -s -X GET "$BASE_URL/api/databases/$DB/status" -H "X-Corelamo-Key: $ADMIN_TOKEN" | jq -r '.data.indexed.documents')
echo "Database '$DB' has $DOCUMENTS documents"

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
check "existing search term" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"alpha","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 128" '(.data | length) == 128'
check "query case insensitivity" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"AlPhA","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 128" '(.data | length) == 128'
check "indexing case insensitivity" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"example"}'
    check_json_bool "data should be 1" '(.data | length) == 1'
check "existing search term + regular symbol" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"alpha.","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 128" '(.data | length) == 128'
check "existing search term divided by regular symbol" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"al.pha."}'
    check_json_bool "data should be 0" '(.data | length) == 0'

#check "existing search term + specsymbol"        200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"real_","docs":3}'
#check_json_bool "data should be 0" '(.data | length) == 0'


section "AND"

check "non existing search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":" inserted"}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "existing and not existing search term" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"alpha notreal"}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "OR existing search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"existing alpha"}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "OR existing search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"existing alpha beta"}'
    check_json_bool "data should be 0" '(.data | length) == 0'

check "existing 2 search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"alpha beta","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 64" '(.data | length) == 64'
check "existing 3 search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"alpha gamma beta","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 32" '(.data | length) == 32'
check "existing 3 search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"beta theta gamma","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 32" '(.data | length) == 32'
check "two of the same" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"alpha alpha","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 128" '(.data | length) == 128'

section "Phrase"

check "non existing search term" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\"notreal alpha\""}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "nothing" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\"\""}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "existing but separated search term" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\"example document\""}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "two of the same" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\"alpha alpha\""}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "wrong order" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\"beta alpha\""}'
    check_json_bool "data should be 0" '(.data | length) == 0'

check "existing 2 search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\"alpha beta\"","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 64" '(.data | length) == 64'
check "existing 3 search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\"alpha beta gamma\"","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 32" '(.data | length) == 32'
check "existing search terms + symbols" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\".alpha&*(#&beta.gamma\"","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 32" '(.data | length) == 32'
check "existing search term + stopword" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\"the alpha beta\"","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 64" '(.data | length) == 64'
check "existing search term + stopword" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"\"alpha the beta\"","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 64" '(.data | length) == 64'

section "OR"

check "non existing search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"{greek alphabet}"}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "nothing" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"{}"}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "non existing or nothing" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"{notreal}"}'
    check_json_bool "data should be 0" '(.data | length) == 0'

check "existing or nothing" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"{alpha}","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 128" '(.data | length) == 128'
check "existing or stopword" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"{alpha a}","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 128" '(.data | length) == 128'
check "existing search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"{alpha theta}","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 192" '(.data | length) == 192'
check "existing search terms" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"{theta alpha}","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 192" '(.data | length) == 192'

#section "NOT"
#nav implimentets vel

#check "nothing" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"~"}'
#    check_json_bool "data should be 0" '(.data | length) == 0'
#check "not non existing" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"~notreal","docs":'"$DOCUMENTS"'}'
#    check_json_bool "data should be $DOCUMENTS" '(.data | length) == '$DOCUMENTS''
#check "not stopword" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"~a"}'
#    check_json_bool "data should be 0" '(.data | length) == 0'

#check "not existing" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"~alpha","docs":'"$DOCUMENTS"'}'
#    check_json_bool "data should be 127" '(.data | length) == 127'
#check "not existing" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"alpha ~alpha","docs":'"$DOCUMENTS"'}'
#    check_json_bool "data should be 0" '(.data | length) == 0'
#check "not two existing" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"~alpha ~beta","docs":'"$DOCUMENTS"'}'
#    check_json_bool "data should be 64" '(.data | length) == 64'
#check "existing not existing" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"alpha ~beta","docs":'"$DOCUMENTS"'}'
#    check_json_bool "data should be 64" '(.data | length) == 64'
#check "existing or not existing" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"{alpha ~beta}","docs":'"$DOCUMENTS"'}'
#    check_json_bool "data should be 193" '(.data | length) == 193'

#hujz
#check "not not existing" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"~~alpha","docs":'"$DOCUMENTS"'}'
#    check_json_bool "data should be 128" '(.data | length) == 128'
#check "not not non existing" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"~~notreal"}'
#    check_json_bool "data should be 0" '(.data | length) == 0'

section "Boolean expressions"

check "gamma or alpha and beta" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"{gamma (alpha beta)}","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 160" '(.data | length) == 160'

section "Wildcard patterns"

check "*" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"*","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be $DOCUMENTS" '(.data | length) == '$DOCUMENTS''

check "3?" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"???","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 163" '(.data | length) == 163'

check "alpha or theta" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"[at][lh][pe][ht][a]","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 192" '(.data | length) == 192'
check "Eta or epsilon or example or eighth" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"e*","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 193" '(.data | length) == 194'
check "everything that has an a in a word" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"*a*","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 256" '(.data | length) == 255'
check "everything that starts with an a(stopword)" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"a*","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 0" '(.data | length) == 0'

check "alpha" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"alph?","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 128" '(.data | length) == 128'

check "shortened version check" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"exampl?","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 1" '(.data | length) == 1'

check "Beta, delta zeta eta theta" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"*[el]ta","docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 248" '(.data | length) == 248'
#check "combo2" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"ch[au]*","docs":'"$DOCUMENTS"'}'
#    check_json_bool "data should be 1" '(.data | length) == 1'

section "Filters"

check "Filter empty brackets" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"*", "filters":{},"docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be $DOCUMENTS" '(.data | length) == '$DOCUMENTS''
check "Filter empty field" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"*", "filters":{"":""},"docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be $DOCUMENTS" '(.data | length) == '$DOCUMENTS''
check "Filter empty query" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"", "filters":{"title":"alpha"},"docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 128" '(.data | length) == 128'
check "Filter empty query, empty filter" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"", "filters":{"title":""},"docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "Filter empty field non empty filter" 409 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"*", "filters":{"":"alpha"},"docs":'"$DOCUMENTS"'}'
check "Filter non indexed" 409 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"alpha", "filters":{"notreal":"first"},"docs":'"$DOCUMENTS"'}'
check "Filter bad syntax" 400 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"alpha", "filters":bad,"docs":'"$DOCUMENTS"'}'

check "Indexed field empty filter" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"alpha", "filters":{"info/text":""},"docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 128" '(.data | length) == 128'
check "Indexed field filter" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"alpha", "filters":{"info/text":"first"},"docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 1" '(.data | length) == 1'
check "Multiple filters" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"*", "filters":{"info/text":"five", "title":"beta"},"docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 35" '(.data | length) == 35'
check "Filter AND" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"alpha", "filters":{"info/text":"one first"},"docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 1" '(.data | length) == 1'
check "Filter OR" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"alpha", "filters":{"info/text":"{two first}"},"docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 8" '(.data | length) == 8'
check "Filter no match" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"alpha", "filters":{"info/text":"second"},"docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 0" '(.data | length) == 0'
check "Filter no match" 200 -X POST "$BASE_URL/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN"     -d '{"query":"example", "filters":{"title":"document","info/text":"zero"},"docs":'"$DOCUMENTS"'}'
    check_json_bool "data should be 1" '(.data | length) == 1'

section "Numeric"

echo
echo "=============================================================="
echo " Results: $PASS_COUNT passed, $FAIL_COUNT failed"
echo "=============================================================="

rm -f /tmp/search_test_body.json

if [ "$FAIL_COUNT" -gt 0 ]; then
    exit 1
fi