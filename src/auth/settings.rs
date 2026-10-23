//! 界面设置：强调色、背板、字体三项。
//!
//! 为什么放在 `auth` 而不是单开一个 settings 模块：外观是**账号属性**
//! （存在 `users.theme` / `users.background` / `users.font`），
//! 写入走 `auth::model::set_appearance`，读出来走中间件已经解析好的那一行用户。
//! 单开模块只会多一层转发，六文件模板里 model/service/policy 三样都是空的。
//!
//! 这一层依旧不写库、不判业务权限：登录判定用 `MaybeUser`，写库调 model。

use askama::Template;
use askama_web::WebTemplate;
use axum::Form;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tower_sessions::Session;

use crate::auth::middleware::{self, ClientIp, MaybeUser, MaybeUserRow};
use crate::auth::model as auth_model;
use crate::error::AppError;
use crate::shared::appearance::{self, Appearance, Choice};
use crate::shared::redirect_to;
use crate::state::AppState;

const SETTINGS_PATH: &str = "/settings";

#[derive(Debug, Deserialize)]
pub struct SavedQuery {
    #[serde(default)]
    pub saved: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ThemeForm {
    #[serde(default)]
    pub theme: String,
    #[serde(default)]
    pub background: String,
    #[serde(default)]
    pub font: String,
    pub _csrf: String,
}

/// 一个选项卡。字段全是 `&'static str`（来自 `shared::appearance` 的目录），
/// 不携带任何用户输入，因此拼进 class / 文本都不会带进注入面。
#[derive(Debug, Clone, Copy)]
pub struct ChoiceCard {
    pub key: &'static str,
    pub label: &'static str,
    pub en: &'static str,
    pub hint: &'static str,
    pub selected: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings.html")]
pub struct SettingsTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: Appearance,
    pub nav_active: &'static str,
    pub themes: Vec<ChoiceCard>,
    pub backdrops: Vec<ChoiceCard>,
    pub fonts: Vec<ChoiceCard>,
    pub saved: bool,
    pub error: Option<String>,
}

fn cards(list: &'static [Choice], current: &str) -> Vec<ChoiceCard> {
    list.iter()
        .map(|choice| ChoiceCard {
            key: choice.key,
            label: choice.label,
            en: choice.en,
            hint: choice.hint,
            selected: choice.key == current,
        })
        .collect()
}

/// 未登录 → 带着 next 去登录页；返回当前用户。
fn require_login(user: &MaybeUser) -> Result<&crate::auth::policy::CurrentUser, AppError> {
    user.user().ok_or_else(|| AppError::LoginRequired {
        next: SETTINGS_PATH.to_owned(),
    })
}

async fn render(
    state: &AppState,
    session: &Session,
    row: &MaybeUserRow,
    user: &MaybeUser,
    error: Option<String>,
    saved: bool,
) -> Result<SettingsTemplate, AppError> {
    let look = row.look();
    Ok(SettingsTemplate {
        page_title: "设置 · NEXUS".to_owned(),
        static_base: state.static_base().to_owned(),
        csrf_token: middleware::ensure_csrf(session).await?,
        nav_display_name: row.display_name().to_owned(),
        nav_is_admin: user.is_admin(),
        nav_is_guest: crate::auth::policy::is_guest(user.user()),
        themes: cards(appearance::THEMES, look.theme),
        backdrops: cards(appearance::BACKDROPS, look.backdrop),
        fonts: cards(appearance::FONTS, look.font),
        look,
        nav_active: "settings",
        saved,
        error,
    })
}

pub async fn settings_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Query(query): Query<SavedQuery>,
) -> Result<SettingsTemplate, AppError> {
    require_login(&user)?;
    let saved = query.saved.as_deref() == Some("1");
    render(&state, &session, &row, &user, None, saved).await
}

pub async fn settings_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    client_ip: ClientIp,
    Form(form): Form<ThemeForm>,
) -> Result<Response, AppError> {
    let current_user = require_login(&user)?.clone();
    middleware::require_csrf(&session, Some(&form._csrf)).await?;

    // 空字段 = 这次没提交这一组，沿用库里的现值；非空但不认识 = 真的给了错值，回 422。
    // 这个区分是有意的：只提交一组的表单（将来的"快速切换强调色"按钮）
    // 不该因为没带另外两组就被判非法；而把 `theme=neon` 这种错值默默吃掉才是事故。
    let look = row.look();
    let raw_theme = form.theme.trim();
    let raw_background = form.background.trim();
    let raw_font = form.font.trim();

    let theme = if raw_theme.is_empty() {
        look.theme
    } else {
        raw_theme
    };
    let background = if raw_background.is_empty() {
        look.backdrop
    } else {
        raw_background
    };
    let font = if raw_font.is_empty() {
        look.font
    } else {
        raw_font
    };

    let mut problems: Vec<&str> = Vec::new();
    if !appearance::is_valid_theme(theme) {
        problems.push("强调色");
    }
    if !appearance::is_valid_backdrop(background) {
        problems.push("背板");
    }
    if !appearance::is_valid_font(font) {
        problems.push("字体");
    }

    if !problems.is_empty() {
        let page = render(
            &state,
            &session,
            &row,
            &user,
            Some(format!("这几项的取值不认识：{}", problems.join("、"))),
            false,
        )
        .await?;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    }

    let mut tx = state.pool.begin().await?;
    auth_model::set_appearance(&mut tx, current_user.id, theme, background, font).await?;
    tx.commit().await?;

    // 外观切换不写审计（DESIGN §28.2）：它是纯偏好，不改变可见性或权限。
    // 保留 client_ip 参数，将来真的需要时不用改签名。
    let _ = client_ip;
    Ok(redirect_to("/settings?saved=1"))
}
