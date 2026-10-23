-- 0006_public.sql —— M6 公开区（前厅）
-- 依赖：0001（users）。forum/kb 的表只被「读」，本文件不碰它们。
--
-- 设计说明（与 DESIGN §24.2 一致）：
-- 1. 三张表都是 public 模块自己的写域。public 对 forum/kb/task 是只读依赖，
--    所以「投稿审核通过」不在这里建议题/文档，而是把 target_type/target_id 回填，
--    真正的建内容动作在 M7 由 admin 模块调 forum/kb service 完成。
-- 2. target_type + target_id 是多态目标，**不加外键**：MySQL 无法用一条外键指向两张表；
--    存在性与可见性由 service::resolve_target 每次写前真查（比外键更严）。
-- 3. public_likes 取消赞 = 物理删除，是全项目唯一的物理删除表：
--    点赞不是内容，软删会让 uk_likes_target_user 永远挡住「取消后再点一次」。
--
-- 回滚：
--   DROP TABLE IF EXISTS public_submissions;
--   DROP TABLE IF EXISTS public_likes;
--   DROP TABLE IF EXISTS public_comments;

CREATE TABLE IF NOT EXISTS public_comments (
  id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  target_type VARCHAR(16)     NOT NULL,
  target_id   BIGINT UNSIGNED NOT NULL,
  author_id   BIGINT UNSIGNED NOT NULL,
  body        VARCHAR(4000)   NOT NULL,
  created_at  TIMESTAMP       NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at  TIMESTAMP       NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at  TIMESTAMP       NULL DEFAULT NULL,
  PRIMARY KEY (id),
  KEY idx_comments_target (target_type, target_id, deleted_at, created_at),
  KEY idx_comments_author (author_id),
  CONSTRAINT fk_comments_author FOREIGN KEY (author_id) REFERENCES users (id) ON DELETE RESTRICT
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS public_likes (
  id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  target_type VARCHAR(16)     NOT NULL,
  target_id   BIGINT UNSIGNED NOT NULL,
  user_id     BIGINT UNSIGNED NOT NULL,
  created_at  TIMESTAMP       NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uk_likes_target_user (target_type, target_id, user_id),
  KEY idx_likes_target (target_type, target_id),
  CONSTRAINT fk_likes_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE RESTRICT
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS public_submissions (
  id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  author_id   BIGINT UNSIGNED NOT NULL,
  title       VARCHAR(200)    NOT NULL,
  body        MEDIUMTEXT      NOT NULL,
  status      VARCHAR(16)     NOT NULL DEFAULT 'pending',
  review_note VARCHAR(500)    NULL DEFAULT NULL,
  reviewed_by BIGINT UNSIGNED NULL DEFAULT NULL,
  reviewed_at TIMESTAMP       NULL DEFAULT NULL,
  target_type VARCHAR(16)     NULL DEFAULT NULL,
  target_id   BIGINT UNSIGNED NULL DEFAULT NULL,
  created_at  TIMESTAMP       NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at  TIMESTAMP       NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at  TIMESTAMP       NULL DEFAULT NULL,
  PRIMARY KEY (id),
  KEY idx_submissions_status (status, created_at),
  KEY idx_submissions_author (author_id),
  CONSTRAINT fk_submissions_author FOREIGN KEY (author_id) REFERENCES users (id) ON DELETE RESTRICT,
  CONSTRAINT fk_submissions_reviewer FOREIGN KEY (reviewed_by) REFERENCES users (id) ON DELETE RESTRICT
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
