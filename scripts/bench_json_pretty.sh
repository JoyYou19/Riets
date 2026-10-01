#!/bin/bash

URL="http://localhost:6006/api/databases/movies/search"
QUERIES=("toy story" "spiderman" "tom hanks" "batman and robin" "action")
TOTAL_REQUESTS=1000

run_benchmark() {
    local pretty_val=$1
    local total_server_us=0
    local count=0

    echo "==> Running $TOTAL_REQUESTS requests with pretty=$pretty_val..."

    # Start wall-clock timer for this batch
    local start_time=$(date +%s.%N)

    for ((i=1; i<=TOTAL_REQUESTS; i++)); do
        local q_index=$(( (i - 1) % ${#QUERIES[@]} ))
        local query="${QUERIES[$q_index]}"

        response=$(curl -s -H "Accept: application/json; pretty=$pretty_val" \
            -X POST "$URL" \
            -d "{\"query\": \"$query\", \"return_fields\": {\"extract\": false, \"cast\": false}}")

        time_str=$(echo "$response" | jq -r '.time_taken // empty')

        if [ -n "$time_str" ]; then
            val=$(echo "$time_str" | sed 's/µs//')
            total_server_us=$(awk "BEGIN {print $total_server_us + $val}")
            count=$((count + 1))
        fi

        # Print progress every 200 requests
        if [ $((i % 200)) -eq 0 ]; then
            echo "   Progress: $i / $TOTAL_REQUESTS completed..."
        fi
    done

    # End wall-clock timer
    local end_time=$(date +%s.%N)
    local wall_clock_duration=$(awk "BEGIN {print $end_time - $start_time}")

    # Compute averages and totals
    if [ $count -gt 0 ]; then
        local avg_server_us=$(awk "BEGIN {print $total_server_us / $count}")
        echo "   ----------------------------------------"
        echo "   Results for pretty=$pretty_val (over $count successful requests):"
        echo "   - Total Wall-Clock Time (Real time): ${wall_clock_duration}s"
        echo "   - Total Server Processing Time     : ${total_server_us}µs ($(( $(echo $total_server_us | cut -d'.' -f1) / 1000 ))ms)"
        echo "   - Average Server Time per Request  : ${avg_server_us}µs"
    else
        echo "   Failed to parse any successful responses for pretty=$pretty_val."
    fi
}

echo "Starting Benchmark (1000 requests per mode, alternating queries)"
echo "--------------------------------------------------------"
run_benchmark "false"
echo "--------------------------------------------------------"
run_benchmark "true"
echo "--------------------------------------------------------"
echo "Benchmark complete!"
