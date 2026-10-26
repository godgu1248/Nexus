-- 0014：给审计表补一个按时间排序的索引。
--
-- 为什么要加：
--   audit_logs 是全库唯一**只增不减**的表（append-only，保留期 ≥180 天）。
--   审计页的默认视图是"不带任何筛选，按时间倒序看最近 20 条"，
--   总览页的「最近动作」也是同一个形状：
--
--       SELECT ... FROM audit_logs a ORDER BY a.created_at DESC, a.id DESC LIMIT 20
--
--   而 0001 里只建了三个复合索引，第一个列分别是 actor_id / (target_type,target_id) / action，
--   **没有一个以 created_at 打头**。于是这条最常见的查询只能全表扫描 + filesort，
--   成本随审计行数线性上升——正好是全库唯一会无限增长的那张表。
--
-- 为什么是 (created_at, id) 而不是只 (created_at)：
--   排序键是 `created_at DESC, id DESC`（同一秒内的多条要有稳定顺序，否则翻页会重复/漏行）。
--   索引带上 id 之后，这个 ORDER BY 可以直接顺着索引走完，不需要额外排序。
--   MySQL 8 支持反向扫描升序索引来满足 DESC 排序，所以不必显式写 DESC。
--
-- 回滚：
--   ALTER TABLE audit_logs DROP INDEX idx_audit_created;

ALTER TABLE audit_logs
    ADD KEY idx_audit_created (created_at, id);