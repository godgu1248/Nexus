//! 应用共享状态：连接池 + 配置 + 限速器。
//!
//! 只放"可安全共享"的东西。限速器内部有锁，但对外只是 `Arc`，
//! 它按应用实例一份——测试各建各的，互不干扰。

use std::sync::Arc;

use axum::extract::FromRef;
use sqlx::MySqlPool;

use crate::config::Config;
use crate::shared::ratelimit::{Limiter, Rules};

#[derive(Clone)]
pub struct AppState {
    pub pool: MySqlPool,
    pub config: Arc<Config>,
    /// 写入口的频率限制（注册/上传/评论/投稿）。
    pub rate: Arc<Limiter>,
}

impl AppState {
    pub fn new(pool: MySqlPool, config: Config) -> Self {
        Self {
            pool,
            config: Arc::new(config),
            rate: Arc::new(Limiter::new(Rules::default())),
        }
    }

    /// 换一套限速规则。测试用它把上限压到个位数，
    /// 免得"验证 429"要先打满生产额度。
    pub fn with_rate_rules(mut self, rules: Rules) -> Self {
        self.rate = Arc::new(Limiter::new(rules));
        self
    }

    pub fn is_prod(&self) -> bool {
        self.config.is_prod()
    }

    /// 模板里拼静态资源地址用。保证结尾没有 `/`，模板侧写 `{{ static_base }}/app.css`。
    pub fn static_base(&self) -> &str {
        &self.config.static_base_url
    }
}

// 让 handler 可以直接 `State(pool): State<MySqlPool>` 而不必自己从 AppState 里取，
// 也让后续 auth 的提取器能只声明它真正需要的依赖。
impl FromRef<AppState> for MySqlPool {
    fn from_ref(state: &AppState) -> Self {
        state.pool.clone()
    }
}

impl FromRef<AppState> for Arc<Config> {
    fn from_ref(state: &AppState) -> Self {
        state.config.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::from_source(|key| match key {
            "DATABASE_URL" => Some("mysql://nexus:pw@127.0.0.1:3306/nexus".to_string()),
            "APP_BASE_URL" => Some("http://localhost:8080".to_string()),
            _ => None,
        })
        .expect("配置应合法")
    }

    // 注：sqlx 的池在创建时就要拿 Tokio 上下文（内部会 spawn 维护任务），
    // 所以这几个测试必须是 #[tokio::test]——写成普通 #[test] 会 panic
    // "this functionality requires a Tokio context"。这是实测过的坑。

    #[tokio::test]
    async fn clone_shares_the_same_config_allocation() {
        // connect_lazy 不真正建立连接，所以这个测试不需要数据库。
        let pool = MySqlPool::connect_lazy("mysql://nexus:pw@127.0.0.1:3306/nexus")
            .expect("惰性池创建失败");
        let state = AppState::new(pool, config());
        let cloned = state.clone();

        assert!(
            Arc::ptr_eq(&state.config, &cloned.config),
            "AppState 克隆不应复制配置"
        );
        assert_eq!(state.static_base(), "/static");
        assert!(!state.is_prod());
    }

    #[tokio::test]
    async fn from_ref_hands_out_clones_not_moves() {
        let pool = MySqlPool::connect_lazy("mysql://nexus:pw@127.0.0.1:3306/nexus")
            .expect("惰性池创建失败");
        let state = AppState::new(pool, config());

        let pool_a: MySqlPool = FromRef::from_ref(&state);
        let pool_b: MySqlPool = FromRef::from_ref(&state);
        // 池的克隆共享同一个底层连接管理器。
        assert_eq!(pool_a.size(), pool_b.size());

        let cfg: Arc<Config> = FromRef::from_ref(&state);
        assert!(Arc::ptr_eq(&cfg, &state.config));
    }
}
