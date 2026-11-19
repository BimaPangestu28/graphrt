-- Weekly active installs: distinct installs that sent a report in the last
-- 7 UTC days (today included).
--   wrangler d1 execute graphrt-telemetry --remote --file=queries/weekly_active.sql
SELECT COUNT(DISTINCT install_id) AS weekly_active_installs
FROM reports
WHERE received_day >= date('now', '-6 days');

-- The same, per ISO-like week, for the last 12 weeks.
SELECT strftime('%Y-W%W', received_day) AS week,
       COUNT(DISTINCT install_id)       AS active_installs
FROM reports
WHERE received_day >= date('now', '-84 days')
GROUP BY week
ORDER BY week DESC;

-- Active installs in the last 7 days by version and build.
SELECT version, build, COUNT(DISTINCT install_id) AS installs
FROM reports
WHERE received_day >= date('now', '-6 days')
GROUP BY version, build
ORDER BY installs DESC;
