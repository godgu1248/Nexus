-- Word 文档的 MIME 比图片长得多：`application/vnd.openxmlformats-officedocument.wordprocessingml.document`
-- 有 72 个字符，而 0012 建表时 `mime VARCHAR(64)` 是按图片（`image/png` 之类）设计的列宽，
-- 于是上传 .docx 直接撞 `1406 (22001) Data too long for column 'mime'`——一个纯列宽问题，
-- 在服务端表现为 500，在用户那边表现为"Word 上传一直失败"。
--
-- 128 而不是刚好 72：MIME 还可能有 `; charset=…` 之类的参数，留一倍余量比再改一次表便宜。
ALTER TABLE media MODIFY COLUMN mime VARCHAR(128) NOT NULL;

-- 回滚：
-- ALTER TABLE media MODIFY COLUMN mime VARCHAR(64) NOT NULL;  -- 只在库中没有 docx 记录时可用
