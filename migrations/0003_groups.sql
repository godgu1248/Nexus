-- 0003_groups.sql —— 小组与"小组可见性"的落地
--
-- 命名裁决：表名用 `member_groups` 而不是 `groups`。
-- `GROUPS` 自 MySQL 8.0.2 起是**保留字**（窗口函数 GROUPS 帧），表名叫 groups 会让
-- 每条 SQL 都必须加反引号，漏一个就是语法错误，而错误信息还不会提示"这是保留字"。
-- 名字多四个字母，换掉一整类低级故障。
--
-- 成员表用复合主键 (group_id, user_id)：这张表没有"某一条成员关系"的独立语义，
-- 任何引用都是引用这一对事实；给自增 id 只会多一个没人用的数字，
-- 而且重复插入要额外写去重逻辑——复合主键天然去重。
--
-- 回滚（手工执行）：
--   ALTER TABLE forum_threads DROP FOREIGN KEY fk_threads_group;
--   ALTER TABLE forum_threads DROP INDEX idx_threads_group, DROP COLUMN group_id;
--   ALTER TABLE forum_spaces DROP FOREIGN KEY fk_spaces_group;
--   ALTER TABLE forum_spaces DROP INDEX idx_spaces_group, DROP COLUMN group_id;
--   DROP TABLE IF EXISTS group_members;
--   DROP TABLE IF EXISTS member_groups;

CREATE TABLE IF NOT EXISTS member_groups (
    id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    name VARCHAR(64) NOT NULL,
    slug VARCHAR(64) NOT NULL,
    description VARCHAR(500) NOT NULL DEFAULT '',
    created_by BIGINT UNSIGNED NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    -- 软删：删组时成员行保留，将来查"当年这个组有谁"还有得查。
    deleted_at TIMESTAMP NULL DEFAULT NULL,
    PRIMARY KEY (id),
    UNIQUE KEY uk_member_groups_slug (slug),
    KEY idx_member_groups_live (deleted_at, created_at),
    CONSTRAINT fk_member_groups_created_by
        FOREIGN KEY (created_by) REFERENCES users (id) ON DELETE RESTRICT
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS group_members (
    group_id BIGINT UNSIGNED NOT NULL,
    user_id BIGINT UNSIGNED NOT NULL,
    joined_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (group_id, user_id),
    -- 反向查"我在哪些组"用得到；每请求一次，必须走索引。
    KEY idx_group_members_user (user_id),
    CONSTRAINT fk_group_members_group
        FOREIGN KEY (group_id) REFERENCES member_groups (id) ON DELETE RESTRICT,
    CONSTRAINT fk_group_members_user
        FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE RESTRICT
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;

-- 内容表补 group_id。
-- M2 只给 visibility 留位，没留"属于哪个组"——因为那时 group 分支恒 false，
-- 多一列就是多一个没人写的字段。M3 要让 group 真生效，这一列才成为必需。
-- 可空是刻意的：public / internal 的内容没有组，`NULL` 表示"不属于任何组"，
-- 而 `visibility='group'` 且 `group_id IS NULL` 属于**数据不完整**，policy 对这种情况一律不放行。
ALTER TABLE forum_spaces
    ADD COLUMN group_id BIGINT UNSIGNED NULL AFTER visibility;

ALTER TABLE forum_spaces
    ADD KEY idx_spaces_group (group_id);

ALTER TABLE forum_spaces
    ADD CONSTRAINT fk_spaces_group
        FOREIGN KEY (group_id) REFERENCES member_groups (id) ON DELETE RESTRICT;

ALTER TABLE forum_threads
    ADD COLUMN group_id BIGINT UNSIGNED NULL AFTER visibility;

ALTER TABLE forum_threads
    ADD KEY idx_threads_group (group_id);

ALTER TABLE forum_threads
    ADD CONSTRAINT fk_threads_group
        FOREIGN KEY (group_id) REFERENCES member_groups (id) ON DELETE RESTRICT;
