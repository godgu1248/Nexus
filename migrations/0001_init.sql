-- 0001_init.sql — Nexus M0 地基：用户表 + 审计日志表
-- 目标库：MySQL 8.0（本地开发可用更高版本，但 SQL 只用 8.0 通用语法）
-- 时区：全表时间列均为 UTC 语义；连接层由 after_connect 执行 SET time_zone='+00:00'
--
-- 回滚（**仅限人工执行，不得写入 migrations 目录**）：
--   DROP TABLE IF EXISTS audit_logs;
--   DROP TABLE IF EXISTS users;

CREATE TABLE users (
    id            BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    username      VARCHAR(64)     NOT NULL COMMENT '登录名。页面上限 32 字符；列宽 64 是为软删除改名 原名__deleted__{id} 预留',
    email         VARCHAR(255)    NOT NULL COMMENT '邮箱。页面上限 191 字符；列宽 255 兼顾唯一索引长度与软删除改名预留',
    password_hash VARCHAR(255)    NOT NULL COMMENT 'argon2id PHC 编码串。软删除时置空字符串，不使用 NULL',
    display_name  VARCHAR(64)     NOT NULL COMMENT '展示名。注册时缺省取 username',
    role          VARCHAR(16)     NOT NULL DEFAULT 'user'   COMMENT 'user | member | admin',
    status        VARCHAR(16)     NOT NULL DEFAULT 'active' COMMENT 'active | pending | suspended | deleted',
    created_at    TIMESTAMP       NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at    TIMESTAMP       NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uk_users_username (username),
    UNIQUE KEY uk_users_email (email),
    KEY idx_users_status_role (status, role),
    KEY idx_users_created_at (created_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci COMMENT='用户。软删除=status:deleted，禁物理删除';

-- 审计日志：append-only。只有 created_at，刻意的——没有 updated_at 就没有「可更新」的语义暗示。
-- actor_id / target_id 不建外键：审计必须比被审计对象活得久，用户软删除后仍要能追溯。
-- 禁止 UPDATE / DELETE / 触发器，靠应用层纪律 + 只给 INSERT/SELECT 的运维习惯保证。
CREATE TABLE audit_logs (
    id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    actor_id    BIGINT UNSIGNED NULL     COMMENT '操作者 users.id。系统动作/游客动作为 NULL。无外键',
    action      VARCHAR(64)     NOT NULL COMMENT '动作常量，见 shared::audit 的 action 常量表',
    target_type VARCHAR(32)     NOT NULL COMMENT '目标类型：user | thread | post | doc | task | ...',
    target_id   BIGINT UNSIGNED NULL     COMMENT '目标主键。无外键',
    meta        JSON            NULL     COMMENT '结构化附加信息。禁写入密码/token/请求体原文',
    ip          VARCHAR(45)     NULL     COMMENT '来源 IP，兼容 IPv6 文本表示',
    created_at  TIMESTAMP       NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    KEY idx_audit_actor  (actor_id, created_at),
    KEY idx_audit_target (target_type, target_id, created_at),
    KEY idx_audit_action (action, created_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci COMMENT='审计日志，append-only';
