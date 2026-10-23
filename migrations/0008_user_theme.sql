-- 0008: 每个用户自己的界面主题。
--
-- 裁决：主题存在 users 表的一列里，而不是新建 user_settings 表。
-- 理由：目前只有这一个偏好；单列表比"万能键值表"更难被滥用，也不会出现
-- "设置项没有对应行"这种半状态。将来真到十几个偏好时再拆表，届时是一列拷贝。
--
-- 取值由应用侧 shared::theme::THEMES 白名单约束（VARCHAR 不是 ENUM，符合
-- 全局约定：枚举一律 VARCHAR + Rust enum，禁 MySQL ENUM 与状态表）。
-- 默认 'ink' 与 shared::theme::DEFAULT_THEME 必须一致。
ALTER TABLE users
    ADD COLUMN theme VARCHAR(32) NOT NULL DEFAULT 'ink' AFTER display_name;

-- 回滚：ALTER TABLE users DROP COLUMN theme;
