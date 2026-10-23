# static/ — 静态资源

这个目录**不由 Rust 进程提供**。生产环境由 Caddy 直接 `file_server` 吐出，请求根本不进 tokio：

```caddyfile
# 约定（M0 不交付部署文件，见 DESIGN §3 A6；这里只固定 URL 形态）
handle /static/* {
	uri strip_prefix /static
	root * /opt/nexus/static
	header Cache-Control "public, max-age=31536000, immutable"
	file_server
}
```

为什么这么切：2C2G 的单机跑 SSR，每个静态字节都要占 worker、占 socket、占带宽预算，而这些文件带强缓存后几乎不需要回源。让 Caddy 处理是"用对工具"，不是"省代码"。

页面上引用的路径由 `STATIC_BASE_URL` 拼出（默认 `/static`），见 `templates/layout.html`：

| 文件 | 用途 | 谁改 |
|---|---|---|
| `app.css` | 全站样式（配色与内置错误页一致） | 人 |
| `app.js` | CSRF 头注入 + HTMX 错误提示 | 人 |
| `htmx.min.js` | HTMX 运行时，**引入的第三方文件** | 只替换，不手改 |

## 第三方文件清单

| 文件 | 版本 | 来源 | 大小 | SHA-256 |
|---|---|---|---|---|
| `htmx.min.js` | 2.0.7 | `https://unpkg.com/htmx.org@2.0.7/dist/htmx.min.js` | 51076 B | `60231AE6BA9DB3825EB15A261122D5F55921C4D53B66BF637DC18B4EE27C79F9` |

**为什么把文件放进仓库而不是用 CDN**：站点的 `/static/*` 由自己的域名提供，用户在中国大陆；指向境外 CDN 意味着多一次 DNS、多一次 TLS 握手，且 CDN 挂掉整站交互全废。文件 50 KB，进仓库最省事。

**为什么没有 SRI `integrity` 属性**：同源静态资源，且上面的哈希就是用来核对"仓库里这份是不是官网那份"的，语义上不需要再让浏览器校验一次。真要防篡改，防线在部署机权限，不在 HTML 属性。

### 升级 htmx 的步骤

1. 下载新版本到临时位置。
2. 用 `Get-FileHash -Algorithm SHA256` 记下哈希，与上表对照，确认文件完整。
3. 覆盖 `htmx.min.js`，同步改上表（版本、大小、哈希）。
4. 确认 `htmx.min.js` 内部版本串对得上：`(Get-Content htmx.min.js -Raw).Contains('version:"2.0.7"')` → `True`。
5. 只做**不兼容**的大版本升级（1.x → 2.x）时，才需要回看 `app.js` 里用到的 `htmx:configRequest` / `htmx:responseError` / `htmx:sendError` 事件是否改名。

## 本地怎么看样式

`cargo run` 起的进程**不服务** `/static/*`，所以直接访问 `http://127.0.0.1:8080/` 会是没有样式的裸 HTML。这是设计，不是故障。本地验收有两种办法：

```powershell
# 办法 A：临时静态服务器，把 STATIC_BASE_URL 指过去（不改变生产行为）
python -m http.server 8090 --directory static
$env:STATIC_BASE_URL = "http://127.0.0.1:8090"
cargo run

# 办法 B：本机装 Caddy，用上面的 Caddyfile 片段反代 8080
```

办法 A 顺带验证了 `STATIC_BASE_URL` 这个配置项**真的被用上了**——如果哪天有人把它写死成 `/static`，A 会立刻掉样式。

## 缓存策略

`app.css` / `app.js` / `htmx.min.js` 目前都按"一年 + immutable"缓存（见上面的 Caddyfile）。代价是**改了文件名不变就拿不到新版**。

M0 阶段接受这个代价：文件少、改动跟着发版走、真出问题时清一次 CDN/浏览器缓存即可。等 M2 前端交互变复杂、需要频繁调样式时，再改成"构建时给文件名加内容哈希"（`app.<hash>.css`），那时必须同时改 `layout.html` 的引用方式——这是有意的债，记在 DESIGN §12。
