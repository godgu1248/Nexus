-- 0011：公告与"已读确认"。
--
-- 为什么要单独一张 ack 表而不是在 users 上加一列 last_seen_notice_id：
-- 一条公告读没读是"人 × 公告"的关系，列存不下多对多；
-- 而"最后看过的 id"在补发旧公告时会误判成已读。
--
-- ack 用复合主键 (announcement_id, user_id)：同一个人对同一条点两次就是同一条记录，
-- 不需要额外唯一索引，也不会出现两行。
--
-- 两个外键都是 RESTRICT：公告删不得（软删兜底），用户的已读记录也不能因为删号而消失
-- ——审计与"谁确认过"的可追溯性比省几行更重要。

CREATE TABLE IF NOT EXISTS announcements (
  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
  title VARCHAR(120) NOT NULL,
  body VARCHAR(2000) NOT NULL,
  author_id BIGINT UNSIGNED NOT NULL,
  is_active TINYINT(1) NOT NULL DEFAULT 1,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at TIMESTAMP NULL DEFAULT NULL,
  KEY idx_notice_live (deleted_at, is_active, created_at),
  KEY idx_notice_author (author_id),
  CONSTRAINT fk_notice_author FOREIGN KEY (author_id) REFERENCES users (id) ON DELETE RESTRICT
) ENGINE = InnoDB
  DEFAULT CHARSET = utf8mb4
  COLLATE = utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS announcement_acks (
  announcement_id BIGINT UNSIGNED NOT NULL,
  user_id BIGINT UNSIGNED NOT NULL,
  acked_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (announcement_id, user_id),
  KEY idx_ack_user (user_id),
  CONSTRAINT fk_ack_notice FOREIGN KEY (announcement_id) REFERENCES announcements (id) ON DELETE RESTRICT,
  CONSTRAINT fk_ack_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE RESTRICT
) ENGINE = InnoDB
  DEFAULT CHARSET = utf8mb4
  COLLATE = utf8mb4_unicode_ci;

-- 回滚：
-- DROP TABLE IF EXISTS announcement_acks;
-- DROP TABLE IF EXISTS announcements;
