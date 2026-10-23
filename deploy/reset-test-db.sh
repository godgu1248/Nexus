#!/bin/sh
# 重置测试库：清空所有业务表，让全量测试从干净状态开始。
#
# 为什么需要它：tests/ 共用一个库、并行跑，任何"全表计数"或"第一页找得到"
# 的断言都会随历史数据漂移。实测跑到第三轮时 users 已 395 行、audit_logs 703 行，
# 于是断言开始随机变红。规则很简单——跑全量测试之前先执行一次本脚本。
#
# 为什么不再写死表清单：写死就会忘。加了 announcements/announcement_acks 之后
# 这脚本仍然只清 13 张表，结果 users 被 TRUNCATE（自增回到 1）、
# 而 announcement_acks 里还留着上一轮指向 id=1..N 的确认行，
# 新一轮的测试用户正好撞上这些 id —— 删用户时报 1451，
# 看起来像"夹具写错"，实际是重置漏表。现在改为从 information_schema 现查。
#
# 注意：不能把多条 TRUNCATE 拼给 PREPARE 执行（ERROR 1064），
# 所以这里先取表名，再拼成一条普通多语句 SQL 走 --execute。
#
# 用法：
#   TEST_DB_HOST=127.0.0.1 TEST_DB_PORT=13306 \
#   TEST_DB_USER=nexus TEST_DB_PASSWORD=*** TEST_DB_NAME=nexus_test \
#   sh deploy/reset-test-db.sh

set -eu

: "${TEST_DB_HOST:=127.0.0.1}"
: "${TEST_DB_PORT:=3306}"
: "${TEST_DB_USER:=nexus}"
: "${TEST_DB_PASSWORD:?需要 TEST_DB_PASSWORD}"
: "${TEST_DB_NAME:=nexus_test}"
MYSQL_BIN="${MYSQL_BIN:-mysql}"

run_sql() {
    MYSQL_PWD="$TEST_DB_PASSWORD" "$MYSQL_BIN" \
        --protocol=TCP \
        --host="$TEST_DB_HOST" \
        --port="$TEST_DB_PORT" \
        --user="$TEST_DB_USER" \
        --database="$TEST_DB_NAME" \
        --batch --skip-column-names \
        --execute="$1"
}

TABLES=$(run_sql "SELECT table_name FROM information_schema.tables
                   WHERE table_schema = DATABASE() AND table_name <> '_sqlx_migrations'
                   ORDER BY table_name;")

if [ -z "$TABLES" ]; then
    echo "reset skipped: $TEST_DB_NAME 里没有任何业务表（迁移还没跑过？）"
    exit 0
fi

# 顺序无关紧要：外键检查先关掉，避免 TRUNCATE 被引用关系挡住。
SQL="SET FOREIGN_KEY_CHECKS = 0;"
for t in $TABLES; do
    # Windows 上 mysql 客户端的输出带 CRLF，不剥掉 \r 就会拼出
    # "TRUNCATE TABLE `announcement_acks\r`" → ERROR 1103 Incorrect table name。
    name=$(printf '%s' "$t" | tr -d '\r')
    SQL="$SQL
TRUNCATE TABLE \`$name\`;"
done
SQL="$SQL
SET FOREIGN_KEY_CHECKS = 1;"

run_sql "$SQL"

COUNT=$(echo "$TABLES" | wc -l | tr -d ' ')
echo "reset ok: $TEST_DB_NAME ($COUNT tables, 由 information_schema 动态发现)"
