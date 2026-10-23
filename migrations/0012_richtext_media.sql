-- 富文本（Word 式编辑器）与图片上传。
--
-- 三张正文表各加一列 body_html：
--   * body     仍然保留，存**纯文本**（编辑器提交时从 HTML 抽出来），搜索、摘要、纯文本渲染都靠它；
--   * body_html 存**净化后的 HTML**，只有 shared::richtext::sanitize 能产出这种值。
-- 这样即使某个模板忘了走净化，回退渲染（转义纯文本）也不会把用户 HTML 当标签执行。
--
-- announcements.body 原来是 VARCHAR(2000)，Word 式正文放不下，顺带放宽到 MEDIUMTEXT。
--
-- media 表按内容寻址：sha256 唯一，用于去重（同一张图重复粘贴只落一份）与不可枚举的 URL。

ALTER TABLE kb_docs
    ADD COLUMN body_html MEDIUMTEXT NULL AFTER body;

ALTER TABLE forum_posts
    ADD COLUMN body_html MEDIUMTEXT NULL AFTER body;

ALTER TABLE announcements
    MODIFY COLUMN body MEDIUMTEXT NOT NULL,
    ADD COLUMN body_html MEDIUMTEXT NULL AFTER body;

CREATE TABLE IF NOT EXISTS media (
    id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    sha256      CHAR(64)        NOT NULL,
    ext         VARCHAR(8)      NOT NULL,
    mime        VARCHAR(64)     NOT NULL,
    bytes       INT UNSIGNED    NOT NULL,
    uploader_id BIGINT UNSIGNED NOT NULL,
    created_at  TIMESTAMP       NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uk_media_sha (sha256),
    KEY idx_media_uploader (uploader_id),
    KEY idx_media_created (created_at),
    CONSTRAINT fk_media_uploader FOREIGN KEY (uploader_id) REFERENCES users (id) ON DELETE RESTRICT
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;

-- 回滚：
-- DROP TABLE IF EXISTS media;
-- ALTER TABLE announcements DROP COLUMN body_html, MODIFY COLUMN body VARCHAR(2000) NOT NULL;
-- ALTER TABLE forum_posts DROP COLUMN body_html;
-- ALTER TABLE kb_docs DROP COLUMN body_html;
