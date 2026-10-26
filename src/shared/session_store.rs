//! 有界会话存储。
//!
//! `tower_sessions_memory_store::MemoryStore` 的 `load` 会把过期会话**过滤**掉，
//! 但那条记录仍然躺在 `HashMap<Id, Record>` 里，永远不会被移除。于是：
//!
//! - 每个匿名访客打开一次带表单的页面（登录页、注册页、首页），`ensure_csrf`
//!   就会签发一个令牌并落一条 session；
//! - 公网上的扫描器、爬虫、一次性访问都会留下记录；
//! - 进程要跑到重启才清空，而我们是 `Restart=on-failure` + 长期运行。
//!
//! 这不是"理论上可能"，是一条只增不减的曲线：`HashMap` 里每条记录连同
//! `data` 里的 JSON 值、`Id`、过期时间，保守估计 300–600 字节，十万条就是几十 MB，
//! 在一台 2C2G、还跑着 MySQL 容器的机器上属于慢性自杀。
//!
//! 所以这里不直接用 `MemoryStore`，而是自己实现 `SessionStore`：
//!
//! 1. **过期即回收**：`load` 命中过期记录时顺手删掉；`create`/`save` 每分钟做一次
//!    全量 `retain`，把过期的都清掉。
//! 2. **容量上限**：超过 `MAX_SESSIONS` 时，按过期时间从早到晚淘汰，保证内存封顶。
//!    上限不是为了"性能"，是为了"被打的时候不会 OOM 把整机拖死"——被淘汰的会话
//!    表现为用户被登出，比整台机器失联好得多。
//!
//! 接口与 `MemoryStore` 完全一致，`SessionManagerLayer` 不需要任何改动。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tower_sessions::SessionStore;
use tower_sessions::session::{Id, Record};
use tower_sessions::session_store;

/// 两次全量回收之间的最小间隔。
///
/// 每次 `create`/`save` 都扫一遍全表是 O(n)，而 n 可能是几万；改成"至多每分钟一次"，
/// 开销摊薄到几乎为零，代价只是过期记录最多多活一分钟。
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// 会话数量上限。
///
/// 按每条 0.5 KB 估，2 万条约 10 MB——对 2G 机器是安全水位。
/// 正常使用（一个团队几十人、每人一两条会话）离这个数字差着三个数量级，
/// 只有被扫站或者被刷的时候才会撞上。
pub const MAX_SESSIONS: usize = 20_000;

/// 内存会话存储，带过期回收和容量上限。
#[derive(Debug, Clone)]
pub struct BoundedStore {
    inner: Arc<Mutex<Inner>>,
    max_sessions: usize,
}

#[derive(Debug)]
struct Inner {
    map: HashMap<Id, Record>,
    last_sweep: Instant,
}

impl Default for BoundedStore {
    fn default() -> Self {
        Self::new(MAX_SESSIONS)
    }
}

impl BoundedStore {
    pub fn new(max_sessions: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                map: HashMap::new(),
                // 初始化为"刚刚扫过"，避免启动瞬间的空表也触发一次扫描。
                last_sweep: Instant::now(),
            })),
            max_sessions: max_sessions.max(1),
        }
    }

    /// 当前保存的会话条数（含尚未被回收的过期记录）。
    ///
    /// 给测试和运维观测用：如果这个数字长期只增不减，说明回收逻辑没生效。
    pub async fn len(&self) -> usize {
        self.inner.lock().await.map.len()
    }

    /// 一条会话都没有。
    ///
    /// 与 `len` 配套：只提供 `len` 的容器类型在被当集合用时，调用方判断空集
    /// 就得写 `len() == 0`，可读性差且容易写错。
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// 立即执行一次回收，返回被删掉的条数。
    pub async fn prune(&self) -> usize {
        let mut inner = self.inner.lock().await;
        inner.sweep(OffsetDateTime::now_utc(), self.max_sessions)
    }
}

impl Inner {
    /// 回收过期记录 + 按上限淘汰，返回删除条数。
    fn sweep(&mut self, now: OffsetDateTime, max_sessions: usize) -> usize {
        let before = self.map.len();
        self.map.retain(|_, record| record.expiry_date > now);

        // 回收完仍然超限，说明短时间内涌入的会话太多（被刷）。
        // 按过期时间升序淘汰：最早该过期的先走，留下的都是"更可能还在用"的。
        if self.map.len() > max_sessions {
            let mut by_expiry: Vec<(OffsetDateTime, Id)> = self
                .map
                .iter()
                .map(|(id, record)| (record.expiry_date, *id))
                .collect();
            by_expiry.sort_unstable_by_key(|entry| entry.0);
            let overflow = self.map.len() - max_sessions;
            for (_, id) in by_expiry.into_iter().take(overflow) {
                self.map.remove(&id);
            }
        }

        self.last_sweep = Instant::now();
        before - self.map.len()
    }

    /// 写入一条记录，顺带做"到点回收 + 超限淘汰"。
    fn insert(&mut self, record: Record, max_sessions: usize) {
        self.map.insert(record.id, record);
        if self.map.len() > max_sessions || self.last_sweep.elapsed() >= SWEEP_INTERVAL {
            self.sweep(OffsetDateTime::now_utc(), max_sessions);
        }
    }
}

#[async_trait]
impl SessionStore for BoundedStore {
    async fn create(&self, record: &mut Record) -> session_store::Result<()> {
        let mut inner = self.inner.lock().await;
        // ID 碰撞处理与 MemoryStore 一致：重新摇号直到不重复。
        // 128 位随机数撞车概率可以忽略，但"可以忽略"不等于"不会发生"。
        while inner.map.contains_key(&record.id) {
            record.id = Id::default();
        }
        let snapshot = record.clone();
        inner.insert(snapshot, self.max_sessions);
        Ok(())
    }

    async fn save(&self, record: &Record) -> session_store::Result<()> {
        let mut inner = self.inner.lock().await;
        inner.insert(record.clone(), self.max_sessions);
        Ok(())
    }

    async fn load(&self, session_id: &Id) -> session_store::Result<Option<Record>> {
        let mut inner = self.inner.lock().await;
        let now = OffsetDateTime::now_utc();
        match inner.map.get(session_id) {
            Some(record) if record.expiry_date > now => Ok(Some(record.clone())),
            Some(_) => {
                // 过期了：顺手删掉，别等下一次全量扫描。
                // 这一条是懒回收的核心——被访问到的过期会话立刻消失。
                inner.map.remove(session_id);
                Ok(None)
            }
            None => Ok(None),
        }
    }

    async fn delete(&self, session_id: &Id) -> session_store::Result<()> {
        self.inner.lock().await.map.remove(session_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration as TimeDuration;

    fn record(expiry: OffsetDateTime) -> Record {
        Record {
            id: Id::default(),
            data: Default::default(),
            expiry_date: expiry,
        }
    }

    fn offset(hours: i64) -> OffsetDateTime {
        OffsetDateTime::now_utc() + TimeDuration::hours(hours)
    }

    #[tokio::test]
    async fn saved_session_can_be_loaded_back() {
        let store = BoundedStore::default();
        let live = record(offset(1));
        let id = live.id;
        store.save(&live).await.unwrap();

        let loaded = store.load(&id).await.unwrap().expect("应当能读回");
        assert_eq!(loaded.id, id);
        assert_eq!(store.len().await, 1);
    }

    #[tokio::test]
    async fn expired_session_is_removed_when_loaded() {
        let store = BoundedStore::default();
        let dead = record(offset(-1));
        let id = dead.id;
        store.save(&dead).await.unwrap();

        // 关键断言：不是"读不到"，而是"读不到且那条记录也没了"。
        // MemoryStore 只做到前半句，后半句就是那个只增不减的曲线。
        assert!(store.load(&id).await.unwrap().is_none());
        assert_eq!(store.len().await, 0);
    }

    #[tokio::test]
    async fn prune_drops_every_expired_entry() {
        let store = BoundedStore::default();
        for _ in 0..3 {
            store.save(&record(offset(-2))).await.unwrap();
        }
        store.save(&record(offset(2))).await.unwrap();
        assert_eq!(store.len().await, 4);

        let removed = store.prune().await;
        assert_eq!(removed, 3);
        assert_eq!(store.len().await, 1);
    }

    #[tokio::test]
    async fn cap_evicts_the_soonest_expiring_sessions() {
        let store = BoundedStore::new(2);
        let early = record(offset(1));
        let early_id = early.id;
        let middle = record(offset(2));
        let middle_id = middle.id;
        store.save(&early).await.unwrap();
        store.save(&middle).await.unwrap();

        // 第三条把容量顶破：最该先过期的 early 必须被淘汰，middle 留下。
        store.save(&record(offset(3))).await.unwrap();

        assert_eq!(store.len().await, 2);
        assert!(store.load(&early_id).await.unwrap().is_none());
        assert!(store.load(&middle_id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn delete_removes_the_entry() {
        let store = BoundedStore::default();
        let live = record(offset(1));
        let id = live.id;
        store.save(&live).await.unwrap();
        store.delete(&id).await.unwrap();
        assert_eq!(store.len().await, 0);
    }

    #[tokio::test]
    async fn create_assigns_a_fresh_id_and_stores_it() {
        let store = BoundedStore::default();
        let mut fresh = record(offset(1));
        store.create(&mut fresh).await.unwrap();
        assert!(store.load(&fresh.id).await.unwrap().is_some());
    }
}
