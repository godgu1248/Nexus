# Nexus 枢纽

一套面向中小型企业的内部协作解决方案。三个区：**内厅**（论坛、小组、任务）、**档案**（树形知识库）、**前厅**（对外只读 + 评论点赞 + 投稿待审）。

服务端渲染、无前端工程链：Axum + Askama + HTMX/Alpine，MySQL 8 + SQLx，单二进制 + systemd，静态文件由反向代理直出（生产用 nginx，见 `deploy/nginx.conf`）。

## 技术栈（版本已锁定，见 `DESIGN.md §1`）

| 层 | 选型 |
| --- | --- |
| Web | `axum 0.8.9`、`tower 0.5.3`、`tower-http 0.7.1` |
| 模板 | `askama 0.16.1` + `askama_web 0.16.0`（SSR，默认转义） |
| 数据库 | MySQL 8 + `sqlx 0.9.0`（`runtime-tokio`/`mysql`/`macros`/`migrate`/`chrono`/`tls-rustls-ring-webpki`） |
| 会话 | `tower-sessions 0.15.0` + 内存 store |
| 口令 | `argon2 0.6.0`（argon2id，m=19456 KiB / t=2 / p=1） |
| 运行时 | `tokio 1.54.1`，单二进制 |

## 目录

```
src/
  main.rs        启动：配置 → 日志 → 连接池 → 迁移 → 引导管理员 → 服务
  app.rs         路由装配、/healthz、首页、404、模板层测试
  config.rs      13+2 项环境变量与启动期校验
  error.rs       AppError + IntoResponse，错误页内置（不走模板，避免递归）
  db.rs          连接池（after_connect 强制 time_zone='+00:00'）、迁移、ping
  state.rs       AppState
  shared/        分页、校验、可见性枚举、审计、HTTP 小工具、富文本净化、外观目录
  auth/          注册/登录/登出/session、policy 四函数、中间件提取器、界面设置
  forum/         空间 → 议题 → 回复
  group/         小组与成员
  kb/            树形档案（草稿/发布、议题沉淀、搜索、置顶）
  task/          任务（指派、状态流转、从议题生成）
  public/        前厅（评论、点赞、投稿、待审队列）
  admin/         管理台（成员、申请、投稿、审计查看）
  notice/        公告（发布、受众、导航红点、逐人确认）
  media/         图片与 Word 上传（内容寻址、magic bytes 校验；图片 4 MiB / 文档 8 MiB）
templates/       Askama 模板（layout + 各模块子目录）
migrations/      0001–0017，编译期嵌入二进制
static/          app.css / app.js / editor.js / htmx.min.js（反向代理直出）
tests/           集成测试（走线上路由装配，需 TEST_DATABASE_URL）
deploy/          systemd unit、nginx.conf（Caddyfile 为早期方案，保留作参考）
DESIGN.md        全部契约、裁决与实测记录（本项目的唯一权威文档）
```

## 发行包

每次发布在 [Releases](https://github.com/godgu1248/Nexus/releases) 附四个压缩包与一份校验和，文件名带版本号：

| 包 | 内容 | 用途 |
| --- | --- | --- |
| `nexus-<版本>-source.zip` | `git archive` 导出的完整源码（不含 `target/`、`.env`） | 自己编译、审计代码 |
| `nexus-<版本>-linux-x86_64.zip` / `.tar.gz` | `nexus` 静态二进制（musl，绝对静态）+ `static/` + `deploy/` + 文档 | 目标机直接跑，不需要 glibc |
| `nexus-<版本>-windows-x86_64.zip` | `nexus.exe` + `static/` + `deploy/` + 文档 | 本地试用、内网演示 |
| `SHA256SUMS.txt` | 上述四个包的 SHA-256 | 下载后先 `sha256sum -c` 再解压 |

这些包不是手工上传的：给仓库打一个 `v*` 标签，`.github/workflows/release.yml` 会用仓库自带的 `GITHUB_TOKEN`，从该标签重新构建 Linux（musl 静态）与 Windows 二进制、导出源码包、附上校验和并创建 Release。所以**任何一个 Release 都能由对应标签复现**，维护者不需要持有个人令牌。

到 github.com 的链路不稳定时（超时、TLS 握手失败，表现为「点了没反应」而不是「没有文件」），可以用同一份文件的完整镜像：

```
https://flowrun.cloud/downloads/nexus-<版本>-linux-x86_64.zip
https://flowrun.cloud/downloads/nexus-<版本>-linux-x86_64.tar.gz
https://flowrun.cloud/downloads/nexus-<版本>-windows-x86_64.zip
https://flowrun.cloud/downloads/nexus-<版本>-source.zip
https://flowrun.cloud/downloads/SHA256SUMS.txt
https://flowrun.cloud/downloads/MIRROR.txt
```

镜像里的字节与 Release 资产完全一致（`sha256sum -c` 能过），目录说明见 `MIRROR.txt`。
当前镜像放的是 **v0.1.10**（同一目录只留最新一版，历史版本请到 Release 页取）。
另外 `https://flowrun.cloud/downloads/local-build/` 还放了维护者本机用 `package.ps1` 打的那一套：
源码相同、链接器不同，所以 Linux 二进制字节不一样，带自己的 `SHA256SUMS.txt` 与 `NOTE.txt`。
要「与 Release 逐字节一致」用上面那组，要「维护者本机验证过的那份」用子目录。

两个二进制包都不含 `.env`：配置项见 `.env.example`，`DATABASE_URL` 是唯一必填项。包内 `deploy/` 放着 systemd unit、nginx 配置与上传目录脚本，与线上跑的是同一份。

解压后三行就能跑起来（Windows 把 `./nexus` 换成 `.\nexus.exe`）：

```bash
unzip nexus-0.1.8-linux-x86_64.zip && cd nexus-0.1.8-linux-x86_64
chmod +x nexus                       # zip 从 Windows 打包时不带可执行位
cp .env.example .env && $EDITOR .env      # 至少填 DATABASE_URL
./nexus                              # 默认监听 127.0.0.1:8080
```

`static/` 需要由反向代理直出（Rust 不挂 `/static`），本地试跑可以 `python3 -m http.server 8090 --directory static` 并把 `STATIC_BASE_URL` 指过去。

## 本地运行

1. 准备 MySQL 8（本地 3306 可达），建库与账号：

```sql
CREATE DATABASE nexus CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
CREATE DATABASE nexus_test CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
CREATE USER 'nexus'@'%' IDENTIFIED BY '请换成你自己的口令';
GRANT ALL PRIVILEGES ON nexus.* TO 'nexus'@'%';
GRANT ALL PRIVILEGES ON nexus_test.* TO 'nexus'@'%';
```

2. 连接串**必须带 `ssl-mode`**（原因见 `DESIGN.md §2.6`：MySQL 8 的 `caching_sha2_password` 在明文连接下要 RSA 交换，而 `mysql-rsa` 会带进 rc 依赖，所以本项目走 TLS）：

```bash
cp .env.example .env
# 改 DATABASE_URL 与 APP_BASE_URL；ssl-ca 指向你实例的 ca.pem
cargo run
```

3. 首次启动会自动跑迁移，并在库里一个 admin 都没有时按 `BOOTSTRAP_ADMIN_*` 建第一个管理员（不设就先用普通账号注册，再手工提权）。

打开 `http://127.0.0.1:8080`。**`/static/*` 不由 Rust 提供**，本地没样式是预期的；要看样式：

```bash
python -m http.server 8090 --directory static
# 然后用 STATIC_BASE_URL=http://127.0.0.1:8090 启动，或临时反向代理
```

## 测试

```bash
export TEST_DATABASE_URL='mysql://nexus:口令@127.0.0.1:3306/nexus_test?ssl-mode=verify_ca&ssl-ca=/path/to/ca.pem'
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

当前 **198 个测试**：lib 130 + admin_flow 10 + auth_flow 6 + forum_flow 10 + group_flow 8 + kb_flow 9 + payload_limit 4 + public_flow 10 + task_flow 10 + utc_timestamps 1。

没有 `TEST_DATABASE_URL` 时，依赖数据库的用例会打印「跳过」并返回——**这种情况下全绿不代表验收通过**，验收必须带库跑。

## 里程碑

| 里程碑 | 内容 | 状态 |
| --- | --- | --- |
| M0 | 地基、依赖锁定、迁移、错误层、模板骨架、静态资源、健康检查 | ✅ |
| M1 | 身份（注册/登录/登出/会话）与审计接线、policy 四函数 | ✅ |
| M2 | 论坛：空间 → 议题 → 回复，定论/归档/软删/分页 | ✅ |
| M3 | 小组与 `group` 可见性、组归属贯穿内容 | ✅ |
| M4 | 档案：树形文档、草稿/发布、议题沉淀、搜索 | ✅ |
| M5 | 任务：指派、状态流转、从议题生成 | ✅ |
| M6 | 前厅：公开列表详情、评论点赞、投稿待审 | ✅ |
| M7 | 管理：成员审批、角色/状态、投稿审核、审计查看 | ✅ |
| M8 | 富文本正文与图片上传（档案/论坛/公告）、外观三项设置 | ✅ |
| M9 | 公告受众（全体/团队）、来宾入口收敛 | ✅ |
| M10 | Word 文档：上传 .docx、解析成受控 HTML、站内查看器、粘贴清洗 | ✅ |

每个里程碑的契约、验收条款与实测记录都在 `DESIGN.md`（§17/§18/§20/§21/§22/§23/§24/§25/§41）。

## 功能一览（面向使用者）

| 位置 | 能做什么 |
| --- | --- |
| 内厅 `/` | 站点入口、五块功能卡、当前身份与健康状态 |
| 公告 `/notice` | 管理员发公告（可选「全体可见 / 仅团队」）；团队成员逐条「我知道了」，导航红点清零 |
| 论坛 `/forum` | 分区 → 议题 → 回复；定论、归档、软删；**分区按热度排序**（未删议题 + 未删回复，不再手填排序值） |
| 档案 `/kb` | 树形文档、草稿/发布、搜索；**归档热度排序**（置顶 → 浏览+点赞 → 更新时间），管理员可置顶多条并定顺序 |
| 任务 `/tasks` | 指派、状态流转、从议题生成 |
| Word 文档 | 编辑器可直接上传 `.docx`，正文与图片解析进站内；文档页内嵌只读查看器（`/doc/{sha}`），复制 Word 内容粘贴时自动去私有标记 |
| 小组 `/groups` | 管理员建组、加人；小组可见性内容 |
| 前厅 `/lobby` | 对外只读展示 + 外人评论点赞投稿 |
| 设置 `/settings` | 强调色 / 背板 / 字体，三项都与账号绑定 |

## 界面与主题

- 深色底 + 强调色的单页风格，无前端框架，HTMX 只做渐进增强。
- 主题共 8 套（墨蓝 / 海青 / 苔绿 / 暮橙 / 雾紫 / 蔷薇 / 琥珀 / 石墨），存在 `users.theme` 列，跟着账号走。
- 登录后从导航右侧「设置」进入 `/settings` 选色卡，保存后全站生效；未登录访问会被带去登录页并回到设置页。
- 样式表与脚本都带版本号（`app.css?v=YYYYMMDD`）。nginx 对 `/static/*` 发一年 `immutable`，**改了 CSS 必须改这个版本号**，否则老访客看不到新样式。
- 未加入的小组账号（`role=user`）在导航显示「来宾 Guest」徽章与顶部提示条，指向 `/join`；
  来宾看不到「任务」入口（任务对来宾不可见，点进去只会 403），也看不到「仅团队」的公告。

## 部署

见 [`deploy/README.md`](deploy/README.md)：本机交叉编译出 `x86_64-unknown-linux-musl` 静态二进制（`deploy/build-musl.ps1`，绝对静态、目标机不需要 glibc）→ 上传到 `/opt/nexus` → systemd 起服务 → **nginx 终止 TLS（外挂证书，应用不碰证书）并直出 `/static`** → 冒烟检查（维护者本地脚本，不随仓库发布）。

线上拓扑与踩坑记录见 [`DESIGN.md §27`](DESIGN.md)。

## 设计上的几条硬约定

1. **权限只有一个入口**：`src/auth/policy.rs` 的 `can_view/can_write/can_moderate/can_admin`，纯内存、不做 IO。
2. **审计与业务同事务**：写操作一律 `begin → 写业务 → audit::record → commit`；审计失败业务必须失败（登出与前厅失败登录是显式例外，见 `DESIGN.md §18.9`）。
3. **可见性过滤在 SQL 里**，不是取回来再筛——后者会让分页总数泄漏。
4. **无权与不存在统一 404**，不用 403 告诉对方"这里有个你看不到的东西"。
5. **`READ COMMITTED` 之上只靠唯一索引与条件 UPDATE 保并发**（`mark_join_reviewed ... WHERE status='pending'`、`set_role ... AND role <> ?`）。
6. **永远至少一个 active admin**：不能降级/停用/删除最后一个，也不能改自己。
