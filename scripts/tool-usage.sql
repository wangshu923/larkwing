-- 「不常用的工具要不要改成用到时才加载」的决策数据(AGENT §4.8:前缀准入看这个家的真实使用频率)。
-- 用法:sqlite3 -readonly <数据根>/larkwing.db < scripts/tool-usage.sql   (或拿一份备份包里的库来跑)
-- 只读、只看工具名与计数,不碰聊天内容。
--
-- 「后置候选」= 2026-11-12 量体量时试分的六组(共 31 个工具、约 1.65 万字符,占工具面 54%):
--   文件加工 / 网页办事与下载 / 音视频加工与曲库 / 电脑管家 / 发到手机 / 盯天气。
-- 这份名单只是拿来算账的假设,不是定案。

.headers on
.mode column

CREATE TEMP TABLE deferred(tool TEXT PRIMARY KEY, grp TEXT);
INSERT INTO deferred VALUES
  ('fs_move','文件加工'),('fs_copy','文件加工'),('fs_mkdir','文件加工'),('fs_trash','文件加工'),
  ('fs_write_text','文件加工'),('fs_append','文件加工'),('fs_edit','文件加工'),('fs_undo','文件加工'),
  ('fs_unzip','文件加工'),('fs_zip','文件加工'),('fs_usage','文件加工'),('fs_stat','文件加工'),
  ('web_render','网页办事与下载'),('web_download','网页办事与下载'),('torrent_download','网页办事与下载'),
  ('ffmpeg_run','音视频加工与曲库'),('media_download','音视频加工与曲库'),('lyrics_fetch','音视频加工与曲库'),
  ('read_audio','音视频加工与曲库'),('speak_to_file','音视频加工与曲库'),
  ('print_file','电脑管家'),('system_status','电脑管家'),('startup_list','电脑管家'),('startup_toggle','电脑管家'),
  ('power','电脑管家'),('pdf_to_png','电脑管家'),('qr_decode','电脑管家'),('qr_encode','电脑管家'),
  ('send_file','发到手机'),('send_text','发到手机'),
  ('watch_set','盯天气');

-- 每条工具行归到它所属的那个用户回合(同会话里它前面最近的一条 user 行)
CREATE TEMP TABLE calls AS
SELECT m.id,
       json_extract(m.payload, '$.name') AS tool,
       m.created_at,
       (SELECT MAX(u.id) FROM messages u
         WHERE u.conversation_id = m.conversation_id AND u.role = 'user' AND u.id < m.id) AS turn
FROM messages m
WHERE m.role = 'tool' AND json_extract(m.payload, '$.name') IS NOT NULL;

SELECT '① 总览' AS '';
SELECT (SELECT COUNT(*) FROM messages WHERE role = 'user') AS 用户回合,
       (SELECT COUNT(*) FROM calls) AS 工具调用,
       (SELECT COUNT(DISTINCT tool) FROM calls) AS 用到过的工具数,
       date(MIN(created_at) / 1000, 'unixepoch', 'localtime') AS 起,
       date(MAX(created_at) / 1000, 'unixepoch', 'localtime') AS 止
FROM messages;

SELECT '② 有多少回合碰过后置候选(越低,后置越划算;每碰一次要多一轮 + 整段历史重算一次缓存)' AS '';
SELECT COUNT(DISTINCT turn) AS 用过工具的回合,
       COUNT(DISTINCT CASE WHEN d.tool IS NOT NULL THEN turn END) AS 其中碰过后置候选的,
       ROUND(100.0 * COUNT(DISTINCT CASE WHEN d.tool IS NOT NULL THEN turn END)
             / MAX(1, (SELECT COUNT(*) FROM messages WHERE role = 'user')), 1) AS 占全部用户回合的百分比
FROM calls c LEFT JOIN deferred d ON d.tool = c.tool;

SELECT '③ 按组:多少回合用到、最近一次' AS '';
SELECT d.grp AS 组, COUNT(*) AS 调用, COUNT(DISTINCT c.turn) AS 回合数,
       date(MAX(c.created_at) / 1000, 'unixepoch', 'localtime') AS 最近一次
FROM calls c JOIN deferred d ON d.tool = c.tool
GROUP BY d.grp ORDER BY 回合数 DESC;

SELECT '④ 逐工具(* = 后置候选)' AS '';
SELECT c.tool || CASE WHEN d.tool IS NOT NULL THEN ' *' ELSE '' END AS 工具,
       COUNT(*) AS 调用, COUNT(DISTINCT c.turn) AS 回合数,
       date(MAX(c.created_at) / 1000, 'unixepoch', 'localtime') AS 最近一次
FROM calls c LEFT JOIN deferred d ON d.tool = c.tool
GROUP BY c.tool ORDER BY 调用 DESC;

SELECT '⑤ 近 30 天的模型调用量(估省多少钱用:每次调用常驻前缀里工具面约 2 万 token,后置候选占一半多)' AS '';
SELECT provider_id AS 供应商, COUNT(*) AS 调用次数,
       SUM(input_tokens) AS 输入token, SUM(cache_hit_tokens) AS 其中缓存命中,
       ROUND(100.0 * SUM(cache_hit_tokens) / MAX(1, SUM(input_tokens)), 1) AS 命中占比
FROM usage_rounds
WHERE created_at >= (strftime('%s', 'now') - 30 * 86400) * 1000
GROUP BY provider_id;
