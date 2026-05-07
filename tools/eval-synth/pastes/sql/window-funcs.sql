SELECT
  user_id,
  event_ts,
  event_name,
  ROW_NUMBER() OVER (PARTITION BY user_id ORDER BY event_ts) AS rn,
  LAG(event_ts) OVER (PARTITION BY user_id ORDER BY event_ts) AS prev_ts
FROM events
WHERE event_ts >= NOW() - INTERVAL '7 days'
  AND event_name IN ('signup', 'first_purchase', 'churn')
ORDER BY user_id, event_ts;
