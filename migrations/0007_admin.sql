-- 0007_admin.sql —— M7 管理：成员加入申请
--
-- 为什么到 M7 才建这张表：M1 时没有任何写入方（外人自助注册直接就是 active user），
-- 先建等于先造一张死表，还要在 M1–M6 的每次迁移里惦记它。需求 L68 的
-- 「外人申请加入」是 M7 的成员审批功能，所以表跟着功能一起到。
--
-- 三条设计说明：
-- 1. 表名与列名都是 `join_requests.status`，SQL 里**永远写表前缀**。users 表也有 status，
--    裸写 status 就是靠猜，而这两列的取值集合完全不同（pending/approved/rejected vs active/...）。
-- 2. `user_id` 是 RESTRICT 外键：申请行跟着用户走，用户行又不许物理删除（L74），
--    所以这条外键永远不会阻塞任何合法操作。
-- 3. 同一用户允许有多条历史申请（被驳回后可以再申请），"同时只能有一条 pending"
--    由服务端查一次来保证，**不加唯一索引**——加了会把"重新申请"永久挡住。

CREATE TABLE IF NOT EXISTS join_requests (
    id            BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    user_id       BIGINT UNSIGNED NOT NULL,
    message       VARCHAR(500)    NOT NULL DEFAULT '',
    status        VARCHAR(16)     NOT NULL DEFAULT 'pending',
    review_note   VARCHAR(500)    NOT NULL DEFAULT '',
    reviewed_by   BIGINT UNSIGNED NULL,
    reviewed_at   TIMESTAMP       NULL DEFAULT NULL,
    created_at    TIMESTAMP       NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at    TIMESTAMP       NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    KEY idx_join_status (status, created_at),
    KEY idx_join_user (user_id),
    CONSTRAINT fk_join_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE RESTRICT,
    CONSTRAINT fk_join_reviewer FOREIGN KEY (reviewed_by) REFERENCES users (id) ON DELETE RESTRICT
) ENGINE = InnoDB
  DEFAULT CHARSET = utf8mb4
  COLLATE = utf8mb4_unicode_ci;

-- 回滚：
-- DROP TABLE IF EXISTS join_requests;
