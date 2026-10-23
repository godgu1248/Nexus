# 部署手册（Alibaba Cloud Linux 4 · nginx · 外挂证书）

线上形态：**nginx 终止 TLS（443）→ 反代 127.0.0.1:8080**；`/static/*` 由 nginx 直接读盘；Rust 进程只跑 HTTP、不带证书。数据库是 MySQL 8.0.46（可用容器，本例宿主 13306 → 容器 3306）。

**为什么不是 Caddy**：本机仓库里还留着 `deploy/Caddyfile` 作参考，但目标机 `dnf` 源默认排除 nginx/httpd 又确实能装到 nginx 1.30.4，而 Caddy 需要额外下载二进制；2C2G 上再装一个"自动申请证书"的守护进程，收益不抵它在内存和 80 端口上的要求。证书本来就是外挂（不由应用申请），用 nginx 更直接。这条偏差记在 `DESIGN.md §27`。

---

## 1. 本地交叉编译（Windows → Linux 静态二进制）

目标三元组 `x86_64-unknown-linux-musl`，产物绝对静态（`PT_INTERP=none`），目标机不需要 glibc/musl。

```powershell
rustup target add x86_64-unknown-linux-musl
rustup target list --installed          # 应含 x86_64-unknown-linux-musl
```

C 编译器 / 链接器用 **zig**（预编译包，免装 VS/clang）。zig 一定要放在**纯 ASCII 路径**（`C:\zig\`）：路径含中文时 cc-rs 调包装器会报 `The system cannot find the path specified.`。

取 zig（官网 49 KB/s、GitHub 全超时；npm 国内镜像 9 秒）：

```powershell
npm pack @oven/zig-win32-x64           # registry.npmmirror.com，得到 zig.exe + lib\
```

包装器 `C:\zig\zigcc.js`（Node）解决两件事，然后 `zigcc.bat` / `x86_64-unknown-linux-musl-zigcc.bat` 只是 `node zigcc.js %*`：

1. cc-rs 会传 `--target=x86_64-unknown-linux-musl`，zig 不认（`error: UnknownOperatingSystem`）→ 剥掉，改传 `-target x86_64-linux-musl`。
2. rustc 在 Windows 上把链接参数塞进 `@response` 文件，里面有它自带的 musl CRT（`rcrt1.o` 等），与 zig 的 `crt1.o` 撞 `duplicate symbol: _start` → **展开 `@file`**，按 basename 丢掉 `crt1.o/rcrt1.o/Scrt1.o/crti.o/crtn.o/crtbegin*.o/crtend*.o`，让 zig 提供 crt。

构建：

```powershell
$env:CC_x86_64_unknown_linux_musl  = 'C:\zig\x86_64-unknown-linux-musl-zigcc.bat'
$env:AR_x86_64_unknown_linux_musl  = 'C:\zig\zigar.bat'
$env:CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = 'C:\zig\zigcc.bat'
cargo build --release --target x86_64-unknown-linux-musl
# 产物：target\x86_64-unknown-linux-musl\release\nexus（约 6.9 MB）
```

自检（不看 `file` 命令，直接读 ELF 头）：`e_type=ET_EXEC`、`e_machine=x86-64`、**`PT_INTERP=none`** 才是真静态。

> 服务器上编译是**行不通**的备选：2C2G 机器跑 `--release`（LTO 链接）会被 OOM 杀；给容器加 swap 余量会把整机拖进 swap 抖动，连 sshd 都发不出 banner。**结论：产物一律本地产出。**

## 2. 服务器一次性准备

```bash
useradd --system --create-home --home-dir /opt/nexus --shell /usr/sbin/nologin nexus
mkdir -p /opt/nexus/static /etc/nexus/certs /var/log/nexus
chown -R nexus:nexus /opt/nexus /var/log/nexus
chmod 700 /etc/nexus/certs
```

数据库（容器或本机实例都行；口令换成自己的）：

```sql
CREATE DATABASE IF NOT EXISTS nexus CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
CREATE USER IF NOT EXISTS 'nexus'@'%' IDENTIFIED BY '换成长口令';
GRANT ALL PRIVILEGES ON nexus.* TO 'nexus'@'%';
FLUSH PRIVILEGES;
SELECT VERSION(), @@have_ssl;     -- 8.0.46 / YES
```

连接串必须带 `ssl-mode`，否则**生产环境拒绝启动**（这是 M0 立的规矩）：`mysql://nexus:口令@127.0.0.1:13306/nexus?ssl-mode=required`。口令里出现 `@ : / ?` 要百分号编码。

## 3. 环境变量（`/etc/nexus/nexus.env`，600）

```bash
APP_ENV=prod
BIND_ADDR=127.0.0.1:8080
APP_BASE_URL=https://example.com
DATABASE_URL=mysql://nexus:换成长口令@127.0.0.1:13306/nexus?ssl-mode=required
STATIC_BASE_URL=/static
SESSION_COOKIE_NAME=nexus_session
SESSION_SECURE=true
SESSION_TTL_HOURS=12
REQUEST_BODY_LIMIT=1048576
DB_MAX_CONNECTIONS=5
LOG_LEVEL=info
DISPLAY_TZ_OFFSET=+08:00
TRUST_PROXY=true
```

- `TRUST_PROXY=true` 只在 nginx 真在前面时开——否则攻击者伪造 `X-Forwarded-For` 就能绕过登录限流。
- `BOOTSTRAP_ADMIN_USERNAME` / `BOOTSTRAP_ADMIN_PASSWORD` 只在库里**一个 admin 都没有**时生效，建完即失效；不要留在仓库里。
- **证书不在这个文件里**：证书是 nginx 的外挂物，应用只监听 127.0.0.1。
- 图片上传（DESIGN §32）额外两个变量，都有默认值，不配就是 ./uploads：
  UPLOAD_DIR=/opt/nexus/uploads、UPLOAD_BASE_URL=/uploads。
  目录必须存在且归 
exus 所有，并且要写进 unit 的 ReadWritePaths（ProtectSystem=strict 下不声明就写不进去）——
  这三件事由 deploy/enable-uploads.sh 幂等完成（同时给 nginx 加上 /uploads/ 直出）。

## 4. systemd

```bash
cp deploy/nexus.service /etc/systemd/system/nexus.service
systemctl daemon-reload && systemctl enable --now nexus
systemctl status nexus --no-pager
journalctl -u nexus -n 50 --no-pager      # 首次启动会自动建表/迁移，并可能创建引导管理员
```

迁移是**编译期嵌入二进制**的（`sqlx::migrate!`），不用上传 `migrations/`，也不用单独跑命令。

## 5. nginx + 外挂证书

```bash
dnf --disableexcludes=all install -y nginx     # 该机 dnf.conf 里 exclude= 了 nginx，必须带这个开关
install -m 600 /path/to/privkey.pem   /etc/nexus/certs/privkey.pem
install -m 644 /path/to/fullchain.pem /etc/nexus/certs/fullchain.pem
cp deploy/nginx.conf /etc/nginx/conf.d/nexus.conf   # 把 server_name 换成自己的域名
nginx -t && systemctl enable --now nginx && systemctl reload nginx
```

证书三条硬要求：

- 用**完整链**（`fullchain.pem`，不是 `cert.pem`），否则部分客户端报不受信。
- `privkey.pem` 权限 600，属主 root；nginx master 以 root 启动，能读。
- 证书要覆盖你实际访问的域名。只签了 `DNS:example.com` 的证书**不能用 IP 访问**（`curl --resolve` 到域名才行）。

`deploy/nginx.conf` 已经写好：80 → 301 跳 https；443 只开 TLSv1.2/1.3；`/static/` 直出并带一年缓存；其余反代并补 `X-Real-IP` / `X-Forwarded-For` / `X-Forwarded-Proto`；`client_max_body_size 2m`（应用自己限制 1 MB，nginx 这层留余量以便返回 413 而不是被断开）。

## 6. 上线验收

```bash
curl -sS https://<域名>/healthz; echo                    # {"status":"ok","db":"ok"}
curl -sS -o /dev/null -w '%{http_code}\n' https://<域名>/                       # 200
curl -sS -o /dev/null -w '%{http_code}\n' https://<域名>/static/app.css         # 200 + Cache-Control: max-age=31536000
curl -sS -o /dev/null -w '%{http_code}\n' https://<域名>/secret/path            # 404，页面不含路径/栈/SQL
curl -sS -D - -o /dev/null https://<域名>/login | grep -i set-cookie            # HttpOnly; Secure; SameSite=Lax
```

再手工走一遍：注册 → 登录 → 申请加入 → 管理员审批 → 改角色/停用 → 档案建文档并发布 → 任务指派 → 前厅评论/点赞/投稿 → 管理台采纳投稿 → 审计页能看到上面每一条。

## 7. 升级与回滚

```powershell
# 本地
cargo test
cargo build --release --target x86_64-unknown-linux-musl
```

```bash
# 服务器：原子替换，不要边拷边换
install -o nexus -g nexus -m 755 /root/nexus.new /opt/nexus/nexus.new
mv /opt/nexus/nexus.new /opt/nexus/nexus
systemctl restart nexus && curl -sS http://127.0.0.1:8080/healthz
```

回滚＝把上一版二进制放回去再 restart。**注意**：若新版本已跑过破坏性迁移，回滚二进制 ≠ 回滚数据库，先恢复备份。

## 8. 备份与保留期

```bash
cat >/etc/cron.d/nexus-backup <<'EOF'
10 3 * * * root mysqldump --single-transaction --default-character-set=utf8mb4 -h127.0.0.1 -P13306 -unexus -p'口令' nexus | gzip > /var/backups/nexus-$(date +\%F).sql.gz && find /var/backups -name 'nexus-*.sql.gz' -mtime +14 -delete
EOF
```

`audit_logs` 需求要求保留 **≥180 天**：不要写任何 DELETE 审计行的清理任务。归档用 `CREATE TABLE ... LIKE` + `INSERT ... SELECT` 另存，原表行不动。

## 9. 排障

| 现象 | 先看什么 |
| --- | --- |
| 启动即退出，日志「配置不合法」 | `.env` 缺项；`DATABASE_URL` 是否带 `ssl-mode`；口令特殊字符是否编码 |
| `/healthz` 200 但 `{"db":"timeout"}`/`down` | MySQL 没起或连接串错；`journalctl -u nexus` 里有真实 sqlx 错误 |
| 全站 502 | 应用没起来（`systemctl status nexus`），或 `BIND_ADDR` 不是 `127.0.0.1:8080` |
| 浏览器证书告警 | 是否用了 `fullchain.pem`；访问域名是否在证书 SAN 内 |
| 页面没样式 | nginx 的 `location /static/` 是否用了 `alias /opt/nexus/static/`（末尾斜杠） |
| 所有人突然变游客 | 内存会话重启即失效（设计如此）；或 `SESSION_SECURE=true` 但站点是 http |
| 登录一直密码错 | `BOOTSTRAP_ADMIN_PASSWORD` 只在第一次生效，之后按普通账号登录 |
| `cargo build` 报 `duplicate symbol: _start` | 包装器没展开 `@response` 文件、没过滤掉 rustc 自带的 `rcrt1.o` |
