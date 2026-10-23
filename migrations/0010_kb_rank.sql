-- 0010：档案排序从「手工排序值」改成「置顶 + 互动热度」。
--
-- 为什么拿掉界面上的 sort_order：手工填 0/1/2 是在让作者替读者做排序，
-- 而读者真正关心的是"最近被看、被点赞、被更新"的东西。列留着不动，
-- 是为了历史行里的旧值不丢（回滚时还能还原成老顺序）。
--
-- view_count 用 BIGINT UNSIGNED：计数器只增不减，永远不会出现负数。
-- is_pinned / pin_order 由管理员维护；置顶可以有多条，pin_order 决定它们之间的先后。

ALTER TABLE kb_docs
  ADD COLUMN view_count BIGINT UNSIGNED NOT NULL DEFAULT 0 AFTER is_published,
  ADD COLUMN is_pinned TINYINT(1) NOT NULL DEFAULT 0 AFTER view_count,
  ADD COLUMN pin_order INT NOT NULL DEFAULT 0 AFTER is_pinned;

ALTER TABLE kb_docs
  ADD KEY idx_kb_rank (is_pinned, pin_order, view_count, updated_at);

-- 回滚：
-- ALTER TABLE kb_docs DROP KEY idx_kb_rank;
-- ALTER TABLE kb_docs
--   DROP COLUMN pin_order,
--   DROP COLUMN is_pinned,
--   DROP COLUMN view_count;
