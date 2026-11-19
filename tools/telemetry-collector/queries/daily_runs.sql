-- Runs and serve requests per UTC day over the last 30 days.
--   wrangler d1 execute graphrt-telemetry --remote --file=queries/daily_runs.sql
SELECT day,
       SUM(runs)                  AS runs,
       SUM(serve_requests)        AS serve_requests,
       COUNT(DISTINCT install_id) AS reporting_installs
FROM daily_counts
WHERE day >= date('now', '-30 days')
GROUP BY day
ORDER BY day;
