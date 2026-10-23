-- 0002 论坛：空间 → 议题 → 回复（DESIGN §20.2）
--
-- 回滚（手工执行，sqlx 不走 down）：
--   DROP TABLE IF EXISTS forum_posts;
--   DROP TABLE IF EXISTS forum_threads;
--   DROP TABLE IF EXISTS forum_spaces;
--
-- 三条与需求通则的显式偏离，都在 DESIGN §20.2 写了理由：
-- 1. forum_posts 不带 visibility：回复的可见性只能等于所属议题，单独存一列允许出现
--    "议题 internal、某条回复 public"这种自相矛盾的状态。
-- 2. forum_threads.visibility 建表就有但 M2 只写 'internal'：M3 改可见性时只改代码不改表。
-- 3. 没有 reply_count 列：计数器一定会漂移，中小型企业用 COUNT(*) + 索引足够。

CREATE TABLE IF NOT EXISTS forum_spaces (
  id            BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  name          VARCHAR(64)  NOT NULL,
  slug          VARCHAR(64)  NOT NULL,
  description   VARCHAR(500) NOT NULL DEFAULT '',
  sort_order    INT          NOT NULL DEFAULT 0,
  visibility    VARCHAR(16)  NOT NULL DEFAULT 'internal',
  created_by    BIGINT UNSIGNED NOT NULL,
  created_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at    TIMESTAMP NULL DEFAULT NULL,
  PRIMARY KEY (id),
  UNIQUE KEY uk_spaces_slug (slug),
  KEY idx_spaces_visibility (visibility, deleted_at, sort_order),
  KEY idx_spaces_created_at (created_at),
  CONSTRAINT fk_spaces_creator FOREIGN KEY (created_by) REFERENCES users (id) ON DELETE RESTRICT
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS forum_threads (
  id            BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  space_id      BIGINT UNSIGNED NOT NULL,
  author_id     BIGINT UNSIGNED NOT NULL,
  title         VARCHAR(200) NOT NULL,
  visibility    VARCHAR(16)  NOT NULL DEFAULT 'internal',
  is_pinned     TINYINT(1)   NOT NULL DEFAULT 0,
  is_resolved   TINYINT(1)   NOT NULL DEFAULT 0,
  is_archived   TINYINT(1)   NOT NULL DEFAULT 0,
  last_post_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  created_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at    TIMESTAMP NULL DEFAULT NULL,
  PRIMARY KEY (id),
  KEY idx_threads_space (space_id, deleted_at, is_pinned, last_post_at),
  KEY idx_threads_visibility (visibility, deleted_at, created_at),
  KEY idx_threads_author (author_id),
  CONSTRAINT fk_threads_space  FOREIGN KEY (space_id)  REFERENCES forum_spaces (id) ON DELETE RESTRICT,
  CONSTRAINT fk_threads_author FOREIGN KEY (author_id) REFERENCES users (id)         ON DELETE RESTRICT
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS forum_posts (
  id            BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  thread_id     BIGINT UNSIGNED NOT NULL,
  author_id     BIGINT UNSIGNED NOT NULL,
  body          MEDIUMTEXT NOT NULL,
  created_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at    TIMESTAMP NULL DEFAULT NULL,
  PRIMARY KEY (id),
  KEY idx_posts_thread (thread_id, deleted_at, created_at),
  KEY idx_posts_author (author_id),
  CONSTRAINT fk_posts_thread FOREIGN KEY (thread_id) REFERENCES forum_threads (id) ON DELETE RESTRICT,
  CONSTRAINT fk_posts_author FOREIGN KEY (author_id) REFERENCES users (id)         ON DELETE RESTRICT
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
