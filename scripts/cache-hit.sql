-- 时钟行对前缀缓存的影响:比较 2026-11-06(v0.2.41,〔此刻〕恒带时钟)前后,
-- 「每个回合的第一轮」的缓存命中占比与平均未命中 token。
-- 只看同一会话里不是第一个回合的(首回合本来就只有固定前缀可命中),只看有缓存的供应商。
WITH first_round AS (
  SELECT u.*, ROW_NUMBER() OVER (PARTITION BY conversation_id ORDER BY id) AS turn_no
  FROM usage_rounds u
  WHERE id IN (SELECT MIN(id) FROM usage_rounds WHERE user_msg_id IS NOT NULL GROUP BY user_msg_id)
)
SELECT
  CASE WHEN created_at < strftime('%s','2026-11-06') * 1000 THEN '11-06 之前' ELSE '11-06 之后' END AS 时段,
  provider_id AS 供应商,
  COUNT(*) AS 回合数,
  ROUND(100.0 * SUM(cache_hit_tokens) / SUM(input_tokens), 1) AS 命中占比,
  ROUND(AVG(input_tokens - cache_hit_tokens)) AS 平均未命中token,
  ROUND(AVG(input_tokens)) AS 平均输入token
FROM first_round
WHERE turn_no > 1 AND input_tokens > 0
GROUP BY 1, 2
ORDER BY 2, 1;
