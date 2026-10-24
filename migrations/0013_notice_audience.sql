-- 公告的受众。
--
-- all  = 全体可见：团队成员向来宾、游客都看得到（默认值，兼容既有数据）；
-- team = 仅团队：只有 member / admin 看得到，来宾与游客既看不到正文，
--        也不算进他们的"未读红点"里。
--
-- 为什么用 VARCHAR 而不是 MySQL ENUM：全项目同一个约定（users.role、users.status、
-- 三张内容表的 visibility 都是 VARCHAR + Rust 侧白名单），枚举值改动不需要 DDL。
-- 为什么默认 'all'：加列时表里已有的公告是发给所有人的，默认值就是它们的真实语义，
-- 用 'team' 会让历史公告突然对所有来宾消失。
ALTER TABLE announcements
  ADD COLUMN audience VARCHAR(16) NOT NULL DEFAULT 'all' AFTER body_html;

-- 红点与列表都按 (audience, is_active, deleted_at) 过滤，一条索引覆盖。
ALTER TABLE announcements
  ADD KEY idx_notice_audience (audience, is_active, deleted_at);

-- 回滚：
-- ALTER TABLE announcements DROP KEY idx_notice_audience;
-- ALTER TABLE announcements DROP COLUMN audience;