//! 跨模块共享的小零件：分页、字段校验、可见性、审计入口。
//!
//! 放这里的门槛只有两条：**多个模块都要用**，且**不含业务判断**。
//! 一旦某个函数开始问"这个用户能不能干这件事"，它就不属于这里——那是 `auth::policy`。

pub mod appearance;
pub mod audit;
pub mod http;
pub mod pagination;
pub mod richtext;
pub mod validate;
pub mod visibility;

pub use appearance::{Appearance, Choice};
pub use http::redirect_to;
pub use pagination::{PAGE_SIZE, Page, PageQuery};
pub use richtext::SafeHtml;
pub use validate::FieldError;
pub use visibility::Visibility;

/// 一次性提示消息（flash）。
///
/// M0 只定义形状给模板宏 `shared/flash.html` 用；M1 起由 session 携带，
/// 渲染一次即丢弃（不做历史消息列表——那会让"提示"变成"日志"，用户就不看了）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashMessage {
    /// 语义类别，直接拼进 CSS 类名。**只允许** `success` / `error` / `info` 三个字面量，
    /// 绝不能来自用户输入——它会进 class 属性。
    pub kind: &'static str,
    pub text: String,
}

impl FlashMessage {
    pub fn success(text: impl Into<String>) -> Self {
        Self {
            kind: "success",
            text: text.into(),
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            kind: "error",
            text: text.into(),
        }
    }

    pub fn info(text: impl Into<String>) -> Self {
        Self {
            kind: "info",
            text: text.into(),
        }
    }
}
