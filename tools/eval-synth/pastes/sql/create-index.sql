CREATE INDEX CONCURRENTLY idx_orders_user_status
  ON orders (user_id, status)
  WHERE status IN ('pending', 'shipped');

EXPLAIN ANALYZE
SELECT id, total
FROM orders
WHERE user_id = 4271 AND status = 'pending'
ORDER BY created_at DESC
LIMIT 50;
