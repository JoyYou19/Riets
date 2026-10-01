-- Rotate through your search queries
local queries = { "toy story", "spiderman", "(tom hanks)", "batman and robin", "action" }
local counter = 0

request = function()
    counter                     = counter + 1
    local query                 = queries[(counter % #queries) + 1]

    wrk.method                  = "POST"
    wrk.body                    = '{"query": "' .. query .. '", "return_fields": {"extract": false, "cast": false}}'
    wrk.headers["Content-Type"] = "application/json"
    -- Note: We will inject the "Accept" header dynamically via the command line argument

    return wrk.format(nil)
end
