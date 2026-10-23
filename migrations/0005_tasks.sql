-- M5 任务：只有一张表。
--
-- 三条设计说明（详见 DESIGN §23.2）：
-- 1. visibility / group_id 建表就位但 M5 只写 'internal'：内容类表必须有可见性列，
--    先有列比后面 ALTER 省事；M5 没有可见性编辑入口。
-- 2. due_on 用 DATE 不用 TIMESTAMP：任务是"哪天要交"，不是"几点"。用时间戳会逼着
--    所有人讨论时区，而中小型企业的截止日就是个日期。
-- 3. assignee_id 可空：未指派是正常状态，不是缺数据。用 NULL 表达，不用 0 或空串。
--
-- 回滚：
--   DROP TABLE IF EXISTS tasks;

CREATE TABLE IF NOT EXISTS tasks (
    id               BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    title            VARCHAR(200)    NOT NULL,
    body             MEDIUMTEXT      NULL,
    status           VARCHAR(16)     NOT NULL DEFAULT 'todo',
    assignee_id      BIGINT UNSIGNED NULL,
    created_by       BIGINT UNSIGNED NOT NULL,
    visibility       VARCHAR(16)     NOT NULL DEFAULT 'internal',
    group_id         BIGINT UNSIGNED NULL,
    due_on           DATE            NULL,
    source_thread_id BIGINT UNSIGNED NULL,
    created_at       TIMESTAMP       NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at       TIMESTAMP       NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    deleted_at       TIMESTAMP       NULL DEFAULT NULL,
    PRIMARY KEY (id),
    KEY idx_tasks_status (status, deleted_at, created_at),
    KEY idx_tasks_assignee (assignee_id, status, deleted_at),
    KEY idx_tasks_source (source_thread_id),
    KEY idx_tasks_group (group_id),
    CONSTRAINT fk_tasks_assignee FOREIGN KEY (assignee_id) REFERENCES users (id) ON DELETE RESTRICT,
    CONSTRAINT fk_tasks_creator FOREIGN KEY (created_by) REFERENCES users (id) ON DELETE RESTRICT,
    CONSTRAINT fk_tasks_group FOREIGN KEY (group_id) REFERENCES member_groups (id) ON DELETE RESTRICT,
    CONSTRAINT fk_tasks_thread FOREIGN KEY (source_thread_id) REFERENCES forum_threads (id) ON DELETE RESTRICT
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;
