curl -sS -o /dev/null -w '%{http_code} %{time_total}s %{size_download}B\n' \
  -H 'Authorization: Bearer $TOKEN' \
  -H 'Accept: application/json' \
  'https://api.example.com/v2/users?limit=50&cursor=eyJpZCI6MTAwMH0='

# Repeat 10x for timing variance
for i in $(seq 1 10); do
  curl -sS -o /dev/null -w '%{time_total}\n' "$URL"
done | awk '{s+=$1} END {print "mean:", s/NR}'
