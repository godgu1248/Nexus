-- 0016：媒体表支持"文档"类附件（Word 导入与网页查看器）
--
-- 背景：编辑器原来只接受图片，Word 文档只能靠"复制粘贴"进来，格式全丢。
-- 现在支持上传 .docx：原文件照样内容寻址存盘（下载用），同时存一份解析后的 HTML
-- （网页查看器与"插入到正文"都用它）。
--
-- 三列的取舍：
--   kind  —— 'image' | 'docx'。默认 'image'，历史行自动归类，不需要回填脚本。
--   title —— 文档标题（取 docx 的核心属性或正文第一段），超过 200 字截断；
--            图片这一列留空。
--   html  —— 解析并净化过的正文 HTML，只对 kind='docx' 有值。
--            **存净化结果而不是原始 OOXML**：查看器每次渲染都重新解析一遍既慢，
--            也让"渲染入口"多一条可能漏净化的路径。
--
-- 为什么不再建一张 media_docx 表：docx 与图片共用同一套内容寻址、去重、审计、
-- 上传限速；拆表会让"按 sha 取媒体"变成两次查询 + 一次分支。

ALTER TABLE media
    ADD COLUMN kind VARCHAR(16) NOT NULL DEFAULT 'image' AFTER mime,
    ADD COLUMN title VARCHAR(200) NULL AFTER kind,
    ADD COLUMN html MEDIUMTEXT NULL AFTER title;

-- 查看器按 sha 查，走已有的 uk_media_sha256 唯一索引；不需要新索引。

-- 回滚：
-- ALTER TABLE media DROP COLUMN html, DROP COLUMN title, DROP COLUMN kind;
