#!/usr/bin/env bash
# Per-field analyzer test suite for the movies DB.
# Requires: curl, jq
set -uo pipefail

BASE="http://localhost:6006/api/databases/movies/search"
DOCS=5

run() {
  local label="$1"
  local query="$2"
  local resp
  resp=$(curl -s -X POST "$BASE" -d "$query")

  echo ""
  echo "### $label"
  echo "    query: $query"

  # summary + total
  echo "    $(echo "$resp" | jq -r '.title // empty')"
  echo "    total: $(echo "$resp" | jq -r '.total_hits.value // empty')"

  # hits (title / year / score); also surfaces error titles like PathNotIndexed
  echo "$resp" | jq -r '.data[]? | "    - \(.data.title // .id)   (\(.data.year // "?"))   score=\(.score)"' 2>/dev/null | head -"$DOCS"
}

echo "================ A. per-field stemming ================"
run "A1 free-text 'running' (title no-stem + extract stem)"      '{"query":"running","docs":5}'
run "A2 extract only 'running' (stemmed)"                        '{"query":"running","search_fields":["extract"],"docs":5}'
run "A3 title only 'running' (no stem)"                          '{"query":"running","search_fields":["title"],"docs":5}'
run "A4a title 'car' (singular)"                                 '{"query":"car","search_fields":["title"],"docs":5}'
run "A4b title 'cars' (plural)"                                  '{"query":"cars","search_fields":["title"],"docs":5}'
run "A5a extract 'car' (singular)"                               '{"query":"car","search_fields":["extract"],"docs":5}'
run "A5b extract 'cars' (plural)"                                '{"query":"cars","search_fields":["extract"],"docs":5}'

echo ""
echo "================ B. exact (\$exact keyword copy) ================"
run "B1 exact whole-value 'the matrix'"                          '{"query":{"exact":"the matrix"},"docs":5}'
run "B2 title word 'matrix'"                                     '{"query":"matrix","search_fields":["title"],"docs":5}'

echo ""
echo "================ C. url tokenizer ================"
run "C1 href 'wikipedia'"                                        '{"query":"wikipedia","search_fields":["href"],"docs":5}'
run "C2 href 'en.wikipedia.org'"                                 '{"query":"en.wikipedia.org","search_fields":["href"],"docs":5}'

echo ""
echo "================ D. nested arrays (cast, row-keyed) ================"
run "D1 cast/name 'keanu'"                                       '{"query":"keanu","search_fields":["cast/name"],"docs":5}'
run "D2 cast/surname 'reeves'"                                   '{"query":"reeves","search_fields":["cast/surname"],"docs":5}'
run "D3 full name 'keanu reeves'"                                '{"query":"keanu reeves","docs":5}'

echo ""
echo "================ E. numeric filter ================"
run "E1 'matrix' + year=1999"                                    '{"query":"matrix","filters":{"year":1999},"docs":5}'

echo ""
echo "================ F. fuzzy / typo ================"
run "F1 typo 'matrx'"                                            '{"query":"matrx","docs":5}'
run "F2 explicit fuzzy 'matrx'"                                  '{"query":{"fuzzy":{"value":"matrx"}},"docs":5}'

echo ""
echo "================ G. ascii folding (only if accented names exist) ================"
run "G1 'francois' cast name/surname"                            '{"query":"francois","search_fields":["cast/name","cast/surname"],"docs":5}'

echo ""
echo "done."
