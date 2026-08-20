clear

BASE_URL="http://localhost:6006"
DB="bug"

get_token() {
    curl -s -X POST "$BASE_URL/api/login" -d "$1" | jq -r '.data.token'
}

ADMIN_TOKEN=$(get_token '{"username":"admin","password":"secret"}')


curl -s -X DELETE $BASE_URL/api/databases/$DB/delete-database -H "X-Corelamo-Key: $ADMIN_TOKEN"
curl -X POST "$BASE_URL/api/databases/$DB/create-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"
curl -X POST "$BASE_URL/api/databases/$DB/start-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"

curl -X POST "$BASE_URL/api/databases/$DB/reindex" -H "X-Corelamo-Key: $ADMIN_TOKEN"

sleep 5

curl -s -X GET $BASE_URL/api/databases/$DB/status -H "X-Corelamo-Key: $ADMIN_TOKEN"
curl -s -X DELETE $BASE_URL/api/databases/$DB/delete-database -H "X-Corelamo-Key: $ADMIN_TOKEN"

