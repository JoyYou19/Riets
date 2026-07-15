#!/bin/bash
# Corelamo auth flow — curl command templates
#
# HOW TO USE:
#   1. Start the server first:  cargo run -p core-runtime -- --root-path /tmp
#   2. Copy ONE block at a time into your terminal (not the whole file at once)
#   3. Read the "EXPECT" comment above each command before running it
#   4. Replace any <placeholder> with a real value

BASE_URL="http://localhost:6006"

# ══════════════════════════════════════════════════════════════════
# STEP 1 — LOGIN AS ADMIN
# EXPECT: HTTP/1.1 200 OK, JSON body containing "data": { "token": "..." }
# ══════════════════════════════════════════════════════════════════
echo "=== STEP 1: login as admin ==="
curl -i -X POST "$BASE_URL/api/login" \
  -d '{"username":"admin","password":"secret"}'

# ══════════════════════════════════════════════════════════════════
# STEP 2 — SAME LOGIN, BUT SAVE THE TOKEN TO A VARIABLE
# EXPECT: no visible output, but $ADMIN_TOKEN is now set.
# Run: echo $ADMIN_TOKEN   to confirm it's not empty before continuing.
# ══════════════════════════════════════════════════════════════════
echo "=== STEP 2: login as admin, save token ==="
ADMIN_TOKEN=$(curl -s -X POST "$BASE_URL/api/login" \
  -d '{"username":"admin","password":"secret"}' | jq -r '.data.token')
echo "ADMIN_TOKEN is now: $ADMIN_TOKEN"

# ══════════════════════════════════════════════════════════════════
# STEP 3 — USE THE TOKEN ON A PROTECTED ROUTE (search)
# EXPECT: HTTP/1.1 200 OK with search results.
# If it says "missing or invalid api key" instead, go back to step 2.
# ══════════════════════════════════════════════════════════════════
echo "=== STEP 3: search movies as admin ==="
curl -i -X POST "$BASE_URL/api/databases/movies/search" \
  -H "X-Corelamo-Key: $ADMIN_TOKEN" \
  -d '{"query":"batman", "docs":3}'

# ══════════════════════════════════════════════════════════════════
# STEP 4 — CONFIRM A PROTECTED ROUTE REJECTS A MISSING TOKEN
# EXPECT: 401 Unauthorized — this is a GOOD result, proves it's protected.
# ══════════════════════════════════════════════════════════════════
echo "=== STEP 4: search movies with NO token (should fail) ==="
curl -i -X POST "$BASE_URL/api/databases/movies/search" \
  -d '{"query":"batman", "docs":3}'

# ══════════════════════════════════════════════════════════════════
# STEP 5 — CONFIRM A PROTECTED ROUTE REJECTS A FAKE TOKEN
# EXPECT: 401 Unauthorized, invalid token message.
# ══════════════════════════════════════════════════════════════════
echo "=== STEP 5: search movies with a FAKE token (should fail) ==="
curl -i -X POST "$BASE_URL/api/databases/movies/search" \
  -H "X-Corelamo-Key: not-a-real-token" \
  -d '{"query":"batman", "docs":3}'

# ══════════════════════════════════════════════════════════════════
# STEP 6 — CONFIRM WRONG PASSWORD IS REJECTED AT LOGIN
# EXPECT: 401 Unauthorized, no token returned.
# ══════════════════════════════════════════════════════════════════
echo "=== STEP 6: login with WRONG password (should fail) ==="
curl -i -X POST "$BASE_URL/api/login" \
  -d '{"username":"admin","password":"wrongpassword"}'

# ══════════════════════════════════════════════════════════════════
# STEP 7 — ROLE PERMISSION TEST: viewer vs editor
# Requires "viewer" and "editor" users to already exist.
# EXPECT:
#   viewer search -> 200 OK
#   viewer insert -> 403/permission denied
#   editor insert -> 200 OK
# ══════════════════════════════════════════════════════════════════
echo "=== STEP 7a: login as viewer ==="
VIEWER_TOKEN=$(curl -s -X POST "$BASE_URL/api/login" \
  -d '{"username":"viewer","password":"secret"}' | jq -r '.data.token')
echo "VIEWER_TOKEN is now: $VIEWER_TOKEN"

echo "=== STEP 7b: login as editor ==="
EDITOR_TOKEN=$(curl -s -X POST "$BASE_URL/api/login" \
  -d '{"username":"editor","password":"secret"}' | jq -r '.data.token')
echo "EDITOR_TOKEN is now: $EDITOR_TOKEN"

echo "=== STEP 7c: viewer searches (should succeed) ==="
curl -i -X POST "$BASE_URL/api/databases/movies/search" \
  -H "X-Corelamo-Key: $VIEWER_TOKEN" -d '{"query":"batman", "docs":3}'

echo "=== STEP 7d: viewer tries to insert (should FAIL) ==="
curl -i -X POST "$BASE_URL/api/databases/movies/insert" \
  -H "X-Corelamo-Key: $VIEWER_TOKEN" -d '{"title":"Test"}'

echo "=== STEP 7e: editor tries to insert (should succeed) ==="
curl -i -X POST "$BASE_URL/api/databases/movies/insert" \
  -H "X-Corelamo-Key: $EDITOR_TOKEN" -d '{"title":"Test"}'

# ══════════════════════════════════════════════════════════════════
# REFERENCE — other routes, once you need them
# ══════════════════════════════════════════════════════════════════

# retrieve documents by id
# curl -i -X POST "$BASE_URL/api/databases/<db_name>/retrieve" \
#   -H "X-Corelamo-Key: $TOKEN" \
#   -d '["<id1>", "<id2>", "<id3>"]'

# create a new database
# curl -i -X POST "$BASE_URL/api/databases/<db_name>/create-database" \
#   -H "X-Corelamo-Key: $TOKEN"

# delete a database
# curl -i -X DELETE "$BASE_URL/api/databases/<db_name>/delete-database" \
#   -H "X-Corelamo-Key: $TOKEN"

# list all databases
# curl -i -X GET "$BASE_URL/api/databases" \
#   -H "X-Corelamo-Key: $TOKEN"

# database status/stats
# curl -i -X GET "$BASE_URL/api/databases/<db_name>/status" \
#   -H "X-Corelamo-Key: $TOKEN"

# reindex a database
# curl -i -X POST "$BASE_URL/api/databases/<db_name>/reindex" \
#   -H "X-Corelamo-Key: $TOKEN"

# get/set policy for a database
# curl -i -X GET "$BASE_URL/api/databases/<db_name>/policy" \
#   -H "X-Corelamo-Key: $TOKEN"
#
# curl -i -X POST "$BASE_URL/api/databases/<db_name>/policy" \
#   -H "X-Corelamo-Key: $TOKEN" \
#   -d '{"<policy_field>":"<value>"}'

# create a new user (requires admin token + /api/users route to exist)
# curl -i -X POST "$BASE_URL/api/users" \
#   -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#   -d '{"username":"<new_username>","password":"<new_password>","roles":["<role>"]}'