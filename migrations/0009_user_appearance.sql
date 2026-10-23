-- 0009: 背景与字体偏好。
--
-- 与 0008 的 theme 一样，偏好就是账号属性：三项都存 users 行，不建 user_settings
-- 键值表（理由见 DESIGN §28.2，三项偏好还远没到需要通用表的规模）。
--
-- 为什么分三列而不是一个 JSON：三列各自有 DEFAULT，加字段时回填是数据库默认值，
-- 不需要应用层处理"旧 JSON 没有这个键"。枚举同样走 VARCHAR + Rust 白名单。

ALTER TABLE users
    ADD COLUMN background VARCHAR(32) NOT NULL DEFAULT 'plain' AFTER theme,
    ADD COLUMN font VARCHAR(32) NOT NULL DEFAULT 'sans' AFTER background;

-- 回滚：
-- ALTER TABLE users DROP COLUMN font, DROP COLUMN background;
