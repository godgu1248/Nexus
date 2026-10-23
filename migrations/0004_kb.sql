-- 0004_kb.sql —— M4 知识库（档案）
--
-- 三处设计要点（详见 DESIGN §22.2）：
--   1. parent_id 用自引用外键 + ON DELETE RESTRICT，不存字符串路径：
--      路径一旦改名就要批量重写，重写失败就是半棵树错位。
--   2. is_published 与 published_at 分开存：布尔值回答"现在发没发"，
--      时间戳回答"什么时候发的"，撤回时把时间戳清空。
--   3. 不加 path / depth 冗余列，深度限制在写入时用递归 CTE 校验。

CREATE TABLE kb_docs (
  id           BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  parent_id    BIGINT UNSIGNED NULL,
  author_id    BIGINT UNSIGNED NOT NULL,
  title        VARCHAR(200) NOT NULL,
  body         MEDIUMTEXT NOT NULL,
  visibility   VARCHAR(16) NOT NULL DEFAULT 'internal',
  group_id     BIGINT UNSIGNED NULL,
  sort_order   INT NOT NULL DEFAULT 0,
  is_published TINYINT(1) NOT NULL DEFAULT 0,
  published_at TIMESTAMP NULL DEFAULT NULL,
  created_at   TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at   TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at   TIMESTAMP NULL DEFAULT NULL,
  PRIMARY KEY (id),
  KEY idx_kb_parent (parent_id, deleted_at, sort_order, id),
  KEY idx_kb_visibility (visibility, deleted_at, updated_at),
  KEY idx_kb_group (group_id),
  CONSTRAINT fk_kb_parent FOREIGN KEY (parent_id) REFERENCES kb_docs (id) ON DELETE RESTRICT,
  CONSTRAINT fk_kb_author FOREIGN KEY (author_id) REFERENCES users (id) ON DELETE RESTRICT,
  CONSTRAINT fk_kb_group  FOREIGN KEY (group_id)  REFERENCES member_groups (id) ON DELETE RESTRICT
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

-- 回滚：
-- DROP TABLE IF EXISTS kb_docs;
