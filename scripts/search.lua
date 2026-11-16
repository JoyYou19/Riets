-- search.lua: varied POST search requests against the "fever" database.
-- Usage:
--   WRK_TOKEN=<key> wrk -t1 -c10 -d30s --latency -s search.lua http://HOST:PORT

-- Replace these with words that exist in your fever data.
local queries = { "london", "states", "world", "war", "ocean" , "economics"}
local token = os.getenv("WRK_TOKEN") or ""
local path = "/api/databases/fever/search"

wrk.method = "POST"
wrk.headers["Content-Type"] = "application/json"
wrk.headers["X-Corelamo-Key"] = token

request = function()
  local q = queries[math.random(#queries)]
  return wrk.format(nil, path, nil, '{"query": "' .. q .. '", "docs": 10}')
end

-- Report non-200 responses so a wrong token or body shows up in the output.
response = function(status, headers, body)
  if status ~= 200 then
    print("non-200 response: " .. status)
  end
end