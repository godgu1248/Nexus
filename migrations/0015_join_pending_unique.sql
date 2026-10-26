-- 一个用户只能有一条「待处理」的加入申请。
--
-- 原来的护栏是「先查有没有 pending，再插入」：两次并发点击都能通过检查，
-- 于是库里出现两条 pending —— 管理员会看到重复条目，批准一条之后另一条还挂着。
-- 加一个生成列 + 唯一索引，把不变式放到数据库里：
--
--   pending_user_id = IF(status='pending', user_id, NULL)
--
-- NULL 不参与唯一性比较，所以同一个人可以有多条 approved/rejected 历史，
-- 但在任意时刻最多只有一条 pending。
--
-- 先清掉可能已经存在的重复 pending（保留最新那条）：否则 ALTER 会因为
-- 违反唯一索引直接失败，而迁移失败会让新二进制拒绝启动。

DELETE j FROM join_requests j
JOIN (
    SELECT user_id, MAX(id) AS keep_id
    FROM join_requests
    WHERE status = 'pending'
    GROUP BY user_id
    HAVING COUNT(*) > 1
) dup ON dup.user_id = j.user_id
WHERE j.status = 'pending' AND j.id <> dup.keep_id;

ALTER TABLE join_requests
    ADD COLUMN pending_user_id BIGINT UNSIGNED
        GENERATED ALWAYS AS (IF(status = 'pending', user_id, NULL)) STORED,
    ADD UNIQUE KEY uk_join_pending (pending_user_id);

-- 回滚：ALTER TABLE join_requests DROP INDEX uk_join_pending, DROP COLUMN pending_user_id;
