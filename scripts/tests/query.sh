QUERY=$1
if [ -z "$QUERY" ]; then
    echo "Usage: $0 <query> [<docs>] [<db>]"
    exit 1
fi
echo "Query: $QUERY"
DOCS=$2
if [ -z "$DOCS" ]; then
    DOCS=10
fi
DB=$3
if [ -z "$DB" ]; then
    DB="docs"
fi

ADMIN_TOKEN=$(curl -s -X POST "http://localhost:6006/api/login" -d '{"username":"admin","password":"secret"}' | jq -r '.data.token')

curl -X POST "http://localhost:6006/api/databases/$DB/start-database" -H "X-Corelamo-Key: $ADMIN_TOKEN"

#curl -X POST "http://localhost:6006/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
#    -d "{\"query\":\"$QUERY\",\"docs\":$DOCS}"

curl -X POST "http://localhost:6006/api/databases/$DB/search" -H "X-Corelamo-Key: $ADMIN_TOKEN" \
    -d "{\"query\":$QUERY}"