//! 按「动作 + 键」计数的固定窗口限速器。
//!
//! 为什么放在 `AppState` 里而不是全局静态：每个应用实例一份计数器，
//! 测试各自建 router 就各自干净，不需要 `#[cfg(test)]` 的"清空全局"后门，
//! 也不会有两个测试互相把对方的额度吃光这种事。生产只有一个实例，效果一样。
//!
//! 只用于**写入口**（注册、上传、评论、投稿）。登录的限速不在
//! 这里——它的键要包含用户名（防的是撞库），而且失败计数要写审计，
//! 那套逻辑在 `auth::service` 里，两者不是一回事。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 窗口长度：一小时。
///
/// 选一小时而不是每分钟：这些动作的"正常用量"是按天算的（一天传十几张图、发几条评论），
/// 用分钟窗口会让正常的连续操作（比如挑图后一次性贴五张）撞墙，
/// 而小时窗口对脚本刷量依然是有效抑制。
const WINDOW: Duration = Duration::from_secs(3600);

/// 计数器最多记多少个键。超出后按"最早重置"的顺序淘汰四分之一。
///
/// 不能整体清空：那等于给攻击者一个"把别人的限额也清掉"的把手
/// （同一个教训在 `auth::service` 的容量保护上已经踩过一次）。
const MAX_KEYS: usize = 4_096;

/// 受限制的动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Register,
    Upload,
    Comment,
    Submission,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Register => "注册",
            Self::Upload => "上传",
            Self::Comment => "评论",
            Self::Submission => "投稿",
        }
    }
}

/// 各动作每小时的上限。
///
/// 默认值按"正常人多打几倍也用不完"来定：注册 20（同一出口 IP 一小时注册 20 个号
/// 已经不像真人）、上传 120（一次贴十几张、来回改几次，够用）、
/// 评论 120、投稿 20。它们的意义是挡住脚本刷量，不是限制人。
#[derive(Clone, Copy, Debug)]
pub struct Rules {
    pub register_per_hour: u32,
    pub upload_per_hour: u32,
    pub comment_per_hour: u32,
    pub submission_per_hour: u32,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            register_per_hour: 20,
            upload_per_hour: 120,
            comment_per_hour: 120,
            submission_per_hour: 20,
        }
    }
}

impl Rules {
    /// 把四个上限整体缩小到 `per_hour`，给测试用。
    ///
    /// 生产上限是给人用的，测试要"打三次就撞墙"才能低成本验证 429 这条路径；
    /// 直接在生产值上打 21 次注册既慢又会让断言依赖 argon2 的速度。
    pub fn uniform(per_hour: u32) -> Self {
        Self {
            register_per_hour: per_hour,
            upload_per_hour: per_hour,
            comment_per_hour: per_hour,
            submission_per_hour: per_hour,
        }
    }

    fn limit_for(&self, kind: Kind) -> u32 {
        match kind {
            Kind::Register => self.register_per_hour,
            Kind::Upload => self.upload_per_hour,
            Kind::Comment => self.comment_per_hour,
            Kind::Submission => self.submission_per_hour,
        }
    }
}

struct Bucket {
    count: u32,
    reset_at: Instant,
}

struct Inner {
    buckets: HashMap<(Kind, String), Bucket>,
}

pub struct Limiter {
    rules: Rules,
    inner: Mutex<Inner>,
}

impl Limiter {
    pub fn new(rules: Rules) -> Self {
        Self {
            rules,
            inner: Mutex::new(Inner {
                buckets: HashMap::new(),
            }),
        }
    }

    /// 记一次，超限时返回"还要等多久"。
    ///
    /// 计数与判断在同一把锁里完成：先查后记会让并发请求同时看到"还没超限"，
    /// 二十个并发请求就能穿过去（和 `admin` 里"先查后改"是同一类错误）。
    pub fn hit(&self, kind: Kind, key: &str) -> Result<(), Duration> {
        let limit = self.rules.limit_for(kind);
        if limit == 0 {
            return Err(WINDOW);
        }

        let now = Instant::now();
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if inner.buckets.len() >= MAX_KEYS {
            inner.evict(now, MAX_KEYS);
        }

        let bucket = inner
            .buckets
            .entry((kind, key.to_owned()))
            .or_insert_with(|| Bucket {
                count: 0,
                reset_at: now + WINDOW,
            });

        if bucket.reset_at <= now {
            // 窗口已过：重新开始计数。
            bucket.count = 0;
            bucket.reset_at = now + WINDOW;
        }

        if bucket.count >= limit {
            let retry = bucket.reset_at.saturating_duration_since(now);
            tracing::debug!(
                action = kind.label(),
                limit,
                retry_secs = retry.as_secs(),
                "触发限速"
            );
            return Err(retry);
        }

        bucket.count += 1;
        Ok(())
    }

    /// 当前记了多少个键（观测与测试用）。
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .buckets
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `hit` 的 handler 友好版本：把"还要等多久"翻成 429。
    ///
    /// 等待时间按分钟向上取整——写"请 47 分钟后再试"没人能照做，
    /// 写"请 48 分钟后再试"至少是个能执行的指示。
    pub fn guard(&self, kind: Kind, key: &str) -> Result<(), crate::error::AppError> {
        match self.hit(kind, key) {
            Ok(()) => Ok(()),
            Err(retry) => Err(crate::error::AppError::RateLimited {
                retry_after_secs: u32::try_from(retry.as_secs()).unwrap_or(u32::MAX),
            }),
        }
    }
}

impl Inner {
    /// 先清过期的；仍超额就按"最早重置"淘汰四分之一。
    fn evict(&mut self, now: Instant, cap: usize) {
        self.buckets.retain(|_, bucket| bucket.reset_at > now);
        if self.buckets.len() < cap {
            return;
        }

        let mut resets: Vec<Instant> = self.buckets.values().map(|b| b.reset_at).collect();
        resets.sort_unstable();
        // 淘汰最早的 `cap / 4` 个，保留其余四分之三：
        // 分界线取第 `evict` 早的那个时间点，早于它的一律丢弃。
        let evict = (cap / 4).max(1);
        let cutoff = resets[evict.min(resets.len() - 1)];
        self.buckets.retain(|_, bucket| bucket.reset_at >= cutoff);
        tracing::warn!(
            remaining = self.buckets.len(),
            "限速器键数达到上限，已淘汰最早的一批"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_the_limit_then_reports_a_wait() {
        let limiter = Limiter::new(Rules::uniform(2));

        assert!(limiter.hit(Kind::Comment, "u1").is_ok());
        assert!(limiter.hit(Kind::Comment, "u1").is_ok());

        let retry = limiter.hit(Kind::Comment, "u1").expect_err("第三次应被拒");
        assert!(retry <= WINDOW, "等待时间不该超过窗口");
        assert!(retry > Duration::ZERO);
    }

    #[test]
    fn keys_are_independent_across_users_and_actions() {
        let limiter = Limiter::new(Rules::uniform(1));

        assert!(limiter.hit(Kind::Comment, "u1").is_ok());
        assert!(
            limiter.hit(Kind::Comment, "u1").is_err(),
            "同一用户第二次应被拒"
        );
        assert!(
            limiter.hit(Kind::Comment, "u2").is_ok(),
            "别人的额度不该被占"
        );
        assert!(
            limiter.hit(Kind::Upload, "u1").is_ok(),
            "同一用户的不同动作各算各的"
        );
    }

    #[test]
    fn zero_limit_always_refuses() {
        // 上限配成 0 时应该「全拒」，而不是「全放」——默认值写错时要往安全的方向错。
        let limiter = Limiter::new(Rules::uniform(0));
        assert!(limiter.hit(Kind::Register, "1.2.3.4").is_err());
    }

    #[test]
    fn expired_windows_start_over() {
        let limiter = Limiter::new(Rules::uniform(1));
        assert!(limiter.hit(Kind::Upload, "u9").is_ok());
        assert!(limiter.hit(Kind::Upload, "u9").is_err());

        // 手动把窗口推到过去，模拟时间流逝（不 sleep 一小时）。
        {
            let mut inner = limiter.inner.lock().expect("锁未中毒");
            for bucket in inner.buckets.values_mut() {
                bucket.reset_at = Instant::now() - Duration::from_secs(1);
            }
        }

        assert!(
            limiter.hit(Kind::Upload, "u9").is_ok(),
            "窗口过后应重新可用"
        );
    }

    #[test]
    fn eviction_keeps_the_newest_quarter_instead_of_wiping_everything() {
        let limiter = Limiter::new(Rules::uniform(5));
        {
            let mut inner = limiter.inner.lock().expect("锁未中毒");
            let now = Instant::now();
            for index in 0..MAX_KEYS {
                inner.buckets.insert(
                    (Kind::Comment, format!("k{index}")),
                    Bucket {
                        count: 1,
                        // 越晚插入的键重置越晚，应该活下来。
                        reset_at: now + Duration::from_secs(index as u64 + 1),
                    },
                );
            }
        }

        assert!(limiter.hit(Kind::Comment, "newcomer").is_ok());

        let len = limiter.len();
        assert!(len < MAX_KEYS, "应当淘汰过，实际 {len}");
        assert!(
            len > MAX_KEYS / 2,
            "淘汰应只清掉最早的一批，不能整体清空，实际剩下 {len}"
        );
    }
}
