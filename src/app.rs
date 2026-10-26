//! 路由装配与首页。
//!
//! 这些东西放在库里而不是 `main.rs`，只为一个理由：**集成测试要能复用它**。
//! `tests/` 下的测试只能链接库 crate，装配逻辑留在 bin 里，测试就只能自己再拼一遍路由——
//! 那份"测试专用路由"和线上路由一旦分叉，测试通过就成了纯粹的自我安慰。
//!
//! `main.rs` 现在只剩三件事：读配置、起日志、绑端口。业务与装配都在这里。

use askama::Template;
use askama_web::WebTemplate;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderName, StatusCode},
    response::IntoResponse,
    routing::get,
};
use serde_json::json;
use time::Duration as TimeDuration;
use tower_http::{
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};
use tower_sessions::{Expiry, Session, SessionManagerLayer, cookie::SameSite};

use crate::admin::routes::routes as admin_routes;
use crate::auth::middleware::{MaybeUser, MaybeUserRow, ensure_csrf};
use crate::auth::routes::routes as auth_routes;
use crate::db;
use crate::error::AppError;
use crate::forum::routes::routes as forum_routes;
use crate::group::routes::routes as group_routes;
use crate::kb::routes::routes as kb_routes;
use crate::media::routes::routes as media_routes;
use crate::notice::routes::routes as notice_routes;
use crate::public::routes::routes as public_routes;
use crate::shared::session_store::BoundedStore;
use crate::state::AppState;
use crate::task::routes::routes as task_routes;

/// 请求 ID 头。回写给客户端，出问题时让他把这一串贴给我们就够定位。
const REQUEST_ID_HEADER: &str = "x-request-id";

/// 健康检查里 `SELECT 1` 的等待上限。
///
/// 为什么必须有：数据库挂掉时，连接池会一直等到 `acquire_timeout`（10 秒）才报错。
/// 而探针的超时通常就是这个量级——探针先超时，网关看到的是"连接失败"而不是"503"，
/// 于是分不清"进程死了"和"数据库死了"。实测过：不加这个超时，MySQL 停掉后 curl 拿到的是 000。
const HEALTHZ_DB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

pub fn build_router(state: AppState) -> Router {
    let config = state.config.clone();

    // 内存 session：单进程、零外部依赖（需求禁 Redis）。代价是重启后所有人要重新登录，
    // 已记入 DESIGN §12 技术债——接受它，因为中小型企业的登录成本远低于多一个组件的运维成本。
    //
    // 用自己写的 `BoundedStore` 而不是 `tower_sessions_memory_store::MemoryStore`：
    // 后者的 `load` 只把过期会话过滤掉、从不删除，匿名访客每打开一次表单页就会留下
    // 一条永不回收的记录（详见 `shared/session_store.rs` 的模块注释）。
    let session_layer = SessionManagerLayer::new(BoundedStore::default())
        .with_name(config.session_cookie_name.clone())
        .with_secure(config.session_secure)
        .with_http_only(true)
        // Lax 而不是 Strict：从外部链接点进来时要保持登录态，否则每次跳转都掉登录。
        // CSRF 由表单 token 兜底，不靠 SameSite 单点防御。
        .with_same_site(SameSite::Lax)
        .with_expiry(Expiry::OnInactivity(TimeDuration::hours(
            config.session_ttl_hours,
        )));

    let request_id_header = HeaderName::from_static(REQUEST_ID_HEADER);

    // 表单与 JSON 的上限收在**这一组**路由上：图片上传要 4 MiB，不能跟着一起被切到 1 MiB。
    // 订单即洋葱：先 layer 的在里层，后 layer 的在外层。
    let core = Router::new()
        .route("/healthz", get(healthz))
        .route("/", get(index))
        .merge(auth_routes())
        .merge(forum_routes())
        .merge(group_routes())
        .merge(kb_routes())
        .merge(notice_routes())
        .merge(task_routes())
        .merge(public_routes())
        .merge(admin_routes())
        .layer(RequestBodyLimitLayer::new(config.request_body_limit));

    Router::new()
        .merge(core)
        // 媒体上传自带 DefaultBodyLimit（4 MiB + 边界余量），所以挂在上面那层之外。
        .merge(media_routes())
        // 未匹配到的一律走内置 404 页，不回显请求路径——否则等于给扫描器当回音壁。
        .fallback(not_found)
        // SetRequestId 放最外层，保证走进 TraceLayer 的每个请求都已经有 id。
        .layer(TraceLayer::new_for_http())
        .layer(PropagateRequestIdLayer::new(request_id_header.clone()))
        .layer(SetRequestIdLayer::new(request_id_header, MakeRequestUuid))
        .layer(session_layer)
        .with_state(state)
}

/// 健康检查。**刻意不走 [`AppError`]**：探针只认 200/503，统一错误页会把语义糊掉。
async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    // 超时也算失败：探针要的是"能不能用"，不是"慢慢能不能用"。
    let probe = tokio::time::timeout(HEALTHZ_DB_TIMEOUT, db::ping(&state.pool)).await;

    match probe {
        Ok(Ok(())) => (StatusCode::OK, Json(json!({ "status": "ok", "db": "ok" }))),
        Ok(Err(err)) => {
            // 详情只进日志，不出响应体。
            tracing::error!(error = ?err, "健康检查失败：数据库不可用");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "degraded", "db": "down" })),
            )
        }
        Err(_) => {
            tracing::error!(
                timeout_secs = HEALTHZ_DB_TIMEOUT.as_secs(),
                "健康检查超时：数据库响应过慢"
            );
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "degraded", "db": "timeout" })),
            )
        }
    }
}

/// 首页视图。字段就是 `templates/layout.html` 顶部注释里写死的那份契约，
/// 一个字段都不能少——少一个 Askama 会在编译期报"找不到字段"，这正是我们要的。
///
/// M2 起内厅首页会被论坛首页取代，那时这个结构体跟着搬进 `forum/handler.rs`。
#[derive(Template, WebTemplate)]
#[template(path = "index.html")]
pub struct IndexTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    /// 导航栏显示的昵称，空串 = 游客。见 `templates/layout.html` 顶部的五字段契约。
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub env_label: &'static str,
}

async fn index(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
) -> Result<IndexTemplate, AppError> {
    Ok(IndexTemplate {
        page_title: "内厅 · NEXUS".to_string(),
        static_base: state.static_base().to_string(),
        // 游客也签发：登录表单要它，页面上任何一处 POST 也都要它。
        csrf_token: ensure_csrf(&session).await?,
        nav_display_name: row.display_name().to_string(),
        nav_is_admin: user.is_admin(),
        nav_is_guest: crate::auth::policy::is_guest(user.user()),
        look: row.look(),
        nav_active: "home",
        env_label: if state.is_prod() { "生产" } else { "开发" },
    })
}

/// 未匹配路由。交给统一错误页渲染，保持 404 的对外口径一致。
async fn not_found() -> AppError {
    AppError::NotFound
}

#[cfg(test)]
mod view_tests {
    //! 模板层的验证。这类测试的价值不在"跑通"，而在**锁住契约**：
    //! 静态前缀写错了、转义被 `|safe` 关了、空 token 硬塞进 meta，都会在这里炸。

    use super::*;
    use crate::shared::{FlashMessage, Page};

    fn index_view() -> IndexTemplate {
        IndexTemplate {
            page_title: "内厅 · NEXUS".to_string(),
            static_base: "/static".to_string(),
            csrf_token: String::new(),
            nav_display_name: String::new(),
            nav_is_admin: false,
            nav_is_guest: false,
            look: crate::shared::appearance::Appearance::DEFAULT,
            nav_active: "home",
            env_label: "开发",
        }
    }

    #[test]
    fn layout_links_static_assets_through_the_configured_prefix() {
        let html = index_view().render().expect("首页渲染失败");
        assert!(
            html.contains("href=\"/static/app.css?v="),
            "样式表路径不对（发版靠 ?v= 换缓存键）"
        );
        assert!(
            html.contains("src=\"/static/htmx.min.js\""),
            "HTMX 没引进来"
        );
        assert!(html.contains("src=\"/static/app.js?v="), "应用脚本没引进来");
        assert!(html.contains("<title>内厅 · NEXUS</title>"), "标题拼接不对");
    }

    #[test]
    fn guests_get_a_badge_and_a_banner_but_members_do_not() {
        let mut guest = index_view();
        guest.nav_display_name = "小明".to_string();
        guest.nav_is_guest = true;
        let html = guest.render().expect("渲染失败");
        assert!(html.contains("badge badge--guest"), "来宾必须有标签");
        assert!(html.contains("Guest"), "标签要带英文名 Guest");
        assert!(html.contains("notice notice--guest"), "来宾必须有提示条");
        assert!(html.contains("href=\"/join\""), "提示条要指向加入申请");

        // 成员看得见公开区，但他不是"来宾"——标签只跟角色走，不跟"看得见多少"走。
        let mut member = index_view();
        member.nav_display_name = "小明".to_string();
        member.nav_is_guest = false;
        let html = member.render().expect("渲染失败");
        assert!(!html.contains("notice--guest"), "成员不该看到来宾提示条");
        assert!(!html.contains("badge--guest"), "成员不该被标成来宾");
    }

    #[test]
    fn body_carries_all_three_appearance_classes() {
        use crate::shared::appearance::Appearance;

        let mut view = index_view();
        view.look = Appearance {
            theme: "ocean",
            backdrop: "aurora",
            font: "serif",
        };
        let html = view.render().expect("渲染失败");

        // 三组外观全部落在 body 的 class 上：任何一组没输出，等于那个设置项不生效。
        assert!(
            html.contains("class=\"theme-ocean bg-aurora font-serif\""),
            "body 必须同时带上主题、背板、字体三个 class"
        );
        for attr in [
            "data-theme=\"ocean\"",
            "data-backdrop=\"aurora\"",
            "data-font=\"serif\"",
        ] {
            assert!(html.contains(attr), "缺 {attr}，调试与脚本都靠它读当前外观");
        }

        // 默认外观也要能落地（游客页走 Appearance::DEFAULT）。
        let html = index_view().render().expect("渲染失败");
        assert!(
            html.contains("class=\"theme-ink bg-plain font-sans\""),
            "默认外观也要生效"
        );
    }

    #[test]
    fn nav_marks_only_the_current_section() {
        let html = index_view().render().expect("渲染失败");
        assert!(
            html.contains("class=\"nav-link is-current\" href=\"/\""),
            "首页应把内厅标成当前项"
        );
        assert!(
            !html.contains("class=\"nav-link is-current\" href=\"/forum\""),
            "首页不该把论坛也标成当前项"
        );
    }

    #[test]
    fn empty_csrf_token_must_not_emit_a_meta_tag() {
        let html = index_view().render().expect("渲染失败");
        assert!(
            !html.contains("name=\"csrf-token\""),
            "没有 token 时不能输出空 meta——前端会读到空串并把它塞进请求头"
        );
    }

    #[test]
    fn csrf_meta_appears_once_a_token_exists() {
        let mut view = index_view();
        view.csrf_token = "tok-abc123".to_string();
        let html = view.render().expect("渲染失败");
        assert!(
            html.contains("<meta name=\"csrf-token\" content=\"tok-abc123\">"),
            "有 token 时必须输出 meta"
        );
    }

    #[test]
    fn template_context_is_html_escaped_by_default() {
        let mut view = index_view();
        view.page_title = "<script>alert(1)</script>".to_string();
        let html = view.render().expect("渲染失败");

        assert!(
            !html.contains("<script>alert(1)</script>"),
            "上下文被原样输出，转义失效"
        );
        // Askama 用的是数字实体（`&#60;`），不是 `&lt;`——断言要贴合真实输出，
        // 而不是贴合我以为的输出。
        assert!(
            html.contains("&#60;script&#62;alert(1)&#60;/script&#62;"),
            "转义结果不对：{html}"
        );
    }

    #[test]
    fn navigation_links_every_opened_section() {
        let html = index_view().render().expect("渲染失败");
        // 未开放的模块必须是 span：做成 <a> 点进去就是 404。
        // 每开放一个里程碑，这条测试就跟着挪一格——不是"改测试让它过"，
        // 而是"里程碑变了，验收标准跟着变"。
        // 现状：论坛（M2）、档案（M4）、小组（M3）、任务（M5）、前厅（M6）全部已开放。
        assert!(html.contains("<a class=\"nav-link is-current\" href=\"/\""));
        for opened in ["/forum", "/kb", "/groups", "/tasks", "/lobby"] {
            assert!(
                html.contains(&format!("<a class=\"nav-link\" href=\"{opened}\">")),
                "{opened} 已开放，必须是可点链接：{html}"
            );
        }
        // 六个入口全部开放后，游客视角下不该再残留任何"计划中"的导航项。
        // 管理员那个"管理"入口由 admin_sees_the_admin_entry_but_it_is_not_a_link_yet 单独盯。
        assert!(
            !html.contains("title=\"前厅将于 M6 开放\""),
            "前厅已开放，不该还标成计划中：{html}"
        );
        assert!(
            !html.contains("title=\"档案将于 M4 开放\"")
                && !html.contains("title=\"论坛将于 M2 开放\"")
                && !html.contains("title=\"任务将于 M5 开放\""),
            "已开放的模块不该再标成计划中"
        );
    }

    #[test]
    fn guest_sees_login_and_register_entries() {
        let html = index_view().render().expect("渲染失败");
        assert!(html.contains("href=\"/login\""));
        assert!(html.contains("href=\"/register\""));
        assert!(!html.contains("action=\"/logout\""), "游客不该看到退出按钮");
        assert!(!html.contains(">管理<"), "游客不该看到管理入口");
    }

    #[test]
    fn signed_in_user_sees_name_logout_form_and_csrf_field() {
        let mut view = index_view();
        view.nav_display_name = "阿岚".to_string();
        view.csrf_token = "tok-nav".to_string();
        let html = view.render().expect("渲染失败");

        assert!(html.contains("阿岚"), "昵称没渲染出来");
        assert!(
            html.contains("<form class=\"inline-form\" method=\"post\" action=\"/logout\">"),
            "退出必须是 POST 表单，不能是链接"
        );
        assert!(
            html.contains("name=\"_csrf\" value=\"tok-nav\""),
            "退出表单必须带 CSRF 字段"
        );
        assert!(
            !html.contains("href=\"/login\""),
            "已登录不该再显示登录入口"
        );
    }

    #[test]
    fn admin_sees_a_working_admin_link() {
        let mut view = index_view();
        view.nav_display_name = "阿岚".to_string();
        view.nav_is_admin = true;
        let html = view.render().expect("渲染失败");

        assert!(html.contains(">管理<"), "管理员应看到管理入口");
        assert!(
            html.contains("<a class=\"nav-link\" href=\"/admin\">管理</a>"),
            "M7 起管理入口必须真的能点进去：{html}"
        );
    }

    #[test]
    fn guests_lose_the_tasks_entry_but_keep_the_read_only_ones() {
        let mut view = index_view();
        view.nav_display_name = "小外来宾".to_string();
        view.nav_is_guest = true;
        let html = view.render().expect("渲染失败");

        // 来宾点「任务」必然 403（任务对 role=user 不可见），
        // 与其让他点进去撞一堵墙，不如这个入口根本不存在。
        assert!(
            !html.contains("href=\"/tasks\""),
            "来宾不该看到任务入口：{html}"
        );
        assert!(
            !html.contains("我的任务"),
            "首页的「我的任务」按钮也属于同一类入口：{html}"
        );
        // 论坛/档案/小组/前厅来宾仍然能进（看到公开部分），不能顺手一起藏掉。
        for opened in ["/forum", "/kb", "/groups", "/lobby"] {
            assert!(
                html.contains(&format!("href=\"{opened}\"")),
                "来宾仍应看到 {opened}：{html}"
            );
        }
    }

    #[test]
    fn members_keep_the_tasks_entry() {
        let mut view = index_view();
        view.nav_display_name = "阿岚".to_string();
        let html = view.render().expect("渲染失败");
        assert!(
            html.contains("<a class=\"nav-link\" href=\"/tasks\">任务</a>"),
            "成员必须能看到任务入口：{html}"
        );
        assert!(html.contains("我的任务"), "成员首页保留「我的任务」按钮");
    }

    /// 只用来渲染两个碎片宏的临时模板。列页面还没实现，但宏必须先被真正渲染过一次。
    #[derive(Template)]
    #[template(
        source = r#"{% import "shared/pagination.html" as pager %}{% import "shared/flash.html" as flash %}{% call pager::render(pager_page, query) %}{% endcall %}{% call flash::render(msgs) %}{% endcall %}"#,
        ext = "html"
    )]
    struct Fragments {
        pager_page: Page<u8>,
        query: &'static str,
        msgs: Vec<FlashMessage>,
    }

    fn fragments(page: Page<u8>, query: &'static str, msgs: Vec<FlashMessage>) -> Fragments {
        Fragments {
            pager_page: page,
            query,
            msgs,
        }
    }

    #[test]
    fn pager_renders_links_and_carries_the_filter_query() {
        let page: Page<u8> = Page::new(vec![], 2, 60, 20);
        // 传原始的 `&`，HTML 转义交给 Askama——它会输出 `&#38;`，浏览器解析 href 时还原。
        let html = fragments(page, "q=rust&", vec![])
            .render()
            .expect("分页渲染失败");

        assert!(html.contains("第 2 / 3 页"), "页码显示不对：{html}");
        assert!(
            html.contains("href=\"?q=rust&#38;page=1\""),
            "上一页丢了筛选条件：{html}"
        );
        assert!(
            html.contains("href=\"?q=rust&#38;page=3\""),
            "下一页丢了筛选条件：{html}"
        );
    }

    #[test]
    fn pager_hides_itself_when_there_is_only_one_page() {
        let page: Page<u8> = Page::new(vec![], 1, 5, 20);
        let html = fragments(page, "", vec![]).render().expect("渲染失败");
        assert!(!html.contains("pager"), "单页时不该出现分页控件：{html}");
    }

    #[test]
    fn pager_disables_edges_instead_of_dropping_them() {
        let first: Page<u8> = Page::new(vec![], 1, 60, 20);
        let html = fragments(first, "", vec![]).render().expect("渲染失败");
        assert!(
            html.contains("aria-disabled=\"true\">上一页"),
            "第一页应渲染禁用态：{html}"
        );
        assert!(html.contains("href=\"?page=2\""), "下一页链接不对：{html}");
    }

    #[test]
    fn flash_renders_kind_classes_and_escapes_text() {
        let msgs = vec![
            FlashMessage::success("已保存"),
            FlashMessage::error("<b>炸了</b>"),
        ];
        let page: Page<u8> = Page::new(vec![], 1, 1, 20);
        let html = fragments(page, "", msgs).render().expect("渲染失败");

        assert!(html.contains("flash flash--success"));
        assert!(html.contains("flash flash--error"));
        assert!(
            html.contains("&#60;b&#62;炸了&#60;/b&#62;"),
            "flash 文本必须转义：{html}"
        );
        assert!(!html.contains("<b>炸了</b>"));
    }

    #[test]
    fn flash_renders_nothing_when_empty() {
        let page: Page<u8> = Page::new(vec![], 1, 1, 20);
        let html = fragments(page, "", vec![]).render().expect("渲染失败");
        assert!(!html.contains("flash-list"), "空消息不该渲染容器：{html}");
    }
}
