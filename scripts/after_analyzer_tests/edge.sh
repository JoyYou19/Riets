#!/usr/bin/env bash
# edge_test.sh - safe to run (./edge_test.sh) or source. Everything runs in a
# subshell, so no shell options, variables or functions leak into your session.

main() {
  BASE="${BASE:-http://localhost:6006/api/databases/edge}"
  DOCS=4

  command -v curl >/dev/null 2>&1 || { echo "curl not found"; return 1; }
  command -v jq   >/dev/null 2>&1 || { echo "jq not found";   return 1; }

  # post <endpoint>  (JSON body on stdin) -> response on stdout, never fails
  post() {
    curl -sS -m 20 -X POST "$BASE/$1" \
      -H "Content-Type: application/json" --data-binary @- 2>&1 \
      || echo '{"title":"curl failed (is the server up?)"}'
  }

  # show only the error/title field of a response, or raw text if not JSON
  brief() {
    out=$(printf '%s\n' "$1" | jq -r '.title // .detail // empty' 2>/dev/null) \
      || out="$1"
    [ -n "$out" ] && echo "  -> $out"
  }

  echo "== create database =="
  brief "$(printf '%s' '{"shard_count":1}' | post create-database)"

  echo "== set policy =="
  brief "$(post set-policy <<'EOF'
{
  "analyzers": {
    "$default": { "stemming": "english" },
    "title": { "lowercase": true },
    "html": { "char_filters": [{ "html_strip": {} }], "stemming": "english" },
    "markdown": { "char_filters": [{ "markdown_strip": {} }], "stemming": "english" },
    "url": { "tokenizer": "url", "ascii_folding": true },
    "code": { "tokenizer": "code", "lowercase": false, "preserve_original": true },
    "filepath": { "tokenizer": "filepath" },
    "name": { "ascii_folding": true }
  },
  "document": {
    "id": "id_auto",
    "title": { "$type": "text", "$analyzer": "title", "$exact": true },
    "summary": { "$type": "text" },
    "body_html": { "$type": "text", "$analyzer": "html" },
    "body_markdown": { "$type": "text", "$analyzer": "markdown" },
    "homepage": { "$type": "text", "$analyzer": "url", "$exact": true },
    "source_code": { "$type": "text", "$analyzer": "code" },
    "repo_path": { "$type": "text", "$analyzer": "filepath" },
    "tags": [{ "$type": "text", "$exact": true }],
    "authors": [
      {
        "name": { "$type": "text", "$analyzer": "name", "$exact": true },
        "site": { "$type": "text", "$analyzer": "url" }
      }
    ]
  }
}
EOF
)"

  echo "== insert documents =="
  brief "$(post insert <<'EOF'
[
  {
    "id": "react-dom-server",
    "title": "ReactDOM Server",
    "summary": "Server side rendering for React applications",
    "body_html": "<p>Render <b>React</b> components to static markup on the <em>server</em>.</p>",
    "body_markdown": "# ReactDOMServer\n\nRenders **React** components to *static markup*.",
    "homepage": "https://reactjs.org/docs/react-dom-server.html",
    "source_code": "export function renderToString(element) {\n  return doRender(element, false);\n}",
    "repo_path": "packages/react-dom/server.node.js",
    "tags": ["react", "ssr"],
    "authors": [{ "name": "Dan Abramov", "site": "https://overreacted.io" }]
  },
  {
    "id": "django-rest",
    "title": "Django REST Framework",
    "summary": "Web APIs for Django",
    "body_html": "<div class=\"note\">A powerful and <a href=\"/api\">flexible</a> toolkit.</div>",
    "body_markdown": "## DRF\n\nA **powerful** toolkit for building Web APIs.",
    "homepage": "https://www.django-rest-framework.org",
    "source_code": "def get_queryset(self):\n    return Model.objects.all()",
    "repo_path": "rest_framework/viewsets.py",
    "tags": ["python", "api"],
    "authors": [{ "name": "Tom Christie", "site": "https://www.django-rest-framework.org/community/" }]
  },
  {
    "id": "hugo",
    "title": "Hugo Static Site Generator",
    "summary": "Fast static site generator written in Go",
    "body_html": "<h1>Hugo</h1><p>The world's <strong>fastest</strong> framework for building websites.</p>",
    "body_markdown": "### Hugo\n\nFast static site generator in **Go**.",
    "homepage": "https://gohugo.io",
    "source_code": "func main() {\n\thugo.Build()\n}",
    "repo_path": "commands/hugo.go",
    "tags": ["go", "static-site"],
    "authors": [{ "name": "Bjørn Erik Pedersen", "site": "https://bep.is" }]
  }
]
EOF
)"

  # label :: query-json   (one search per line)
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    label="${line%% :: *}"
    query="${line#* :: }"
    resp=$(printf '%s' "$query" | post search)
    echo ""
    echo "### $label"
    echo "    query: $query"
    err=$(printf '%s\n' "$resp" | jq -r '.title // empty' 2>/dev/null)
    [ -n "$err" ] && echo "    error: $err"
    printf '%s\n' "$resp" \
      | jq -r '.data[]? | "    - \(.data.title // .id)   score=\(.score)"' 2>/dev/null \
      | head -n "$DOCS"
  done <<'EOF'
html: word inside <b> :: {"query":"react","search_fields":["body_html"],"docs":4}
html: link text :: {"query":"flexible","search_fields":["body_html"],"docs":4}
md: word inside ** :: {"query":"react","search_fields":["body_markdown"],"docs":4}
md: word after ## :: {"query":"drf","search_fields":["body_markdown"],"docs":4}
url: host :: {"query":"reactjs.org","search_fields":["homepage"],"docs":4}
url: hyphenated domain :: {"query":"django-rest-framework.org","search_fields":["homepage"],"docs":4}
url: nested authors/site :: {"query":"overreacted.io","search_fields":["authors/site"],"docs":4}
code: camelCase fn :: {"query":"renderToString","search_fields":["source_code"],"docs":4}
code: snake_case fn :: {"query":"get_queryset","search_fields":["source_code"],"docs":4}
code: split word :: {"query":"doRender","search_fields":["source_code"],"docs":4}
fp: filename :: {"query":"viewsets","search_fields":["repo_path"],"docs":4}
fp: full path :: {"query":"hugo.go","search_fields":["repo_path"],"docs":4}
nested authors/name :: {"query":"abramov","search_fields":["authors/name"],"docs":4}
ascii: bjorn matches Bjørn :: {"query":"bjorn","search_fields":["authors/name"],"docs":4}
tags: ssr :: {"query":"ssr","search_fields":["tags"],"docs":4}
exact title :: {"query":{"exact":"reactdom server"},"docs":4}
EOF

  echo ""
  echo "done."
}

# Subshell = nothing leaks into your interactive shell, even if sourced.
( main )
