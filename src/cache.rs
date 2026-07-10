//! syntax span、wrap map、intraline result が共有する byte 制限付き LRU cache。
//!
//! `WeightedLruCache` は resident entry と、cache から追い出された後も caller が
//! `CacheHandle` で保持している value の双方を同じ live-byte budget へ計上する。LRU の順序と
//! resident byte 数は `&mut self` で直列化し、handle の最終 drop は任意 thread から atomic counter
//! を減らす。したがって、この型全体を `Mutex` で包んだ後も lock 外へ value を安全に貸し出せる。
//!
//! key/value の大きさは型から推測しない。caller が insert 時に渡す weight は、その value が
//! handle 経由で生存する間に derived-cache budget へ課す byte 数である。key や LRU bookkeeping の
//! allocation はこの weight に含めるかどうかを caller 側の見積もり規約で決める。

use std::borrow::Borrow;
use std::error::Error;
use std::fmt;
use std::hash::Hash;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lru::LruCache;

/// Insert を拒否した理由。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheInsertRejection {
    /// 単独の entry が cache 全体の byte limit を超えている。
    EntryLargerThanLimit,
    /// Cache 内を LRU 順に空にしても、外部 handle が保持する byte により新規 entry が収まらない。
    LiveBudgetExhausted,
}

/// 拒否された key/value を caller へ返す insert error。
///
/// 失敗時に cache の entry、LRU 順序、accounting は変化しない。caller は [`Self::into_entry`] で
/// allocation 済みの value を回収し、cache 外で利用するか、handle 解放後に再試行できる。
#[derive(Debug)]
pub struct CacheInsertError<K, V> {
    reason: CacheInsertRejection,
    key: K,
    value: V,
    weight_bytes: usize,
    live_bytes: usize,
    limit_bytes: usize,
}

impl<K, V> CacheInsertError<K, V> {
    /// Insert を拒否した理由を返す。
    #[must_use]
    pub fn reason(&self) -> CacheInsertRejection {
        self.reason
    }

    /// 拒否された entry の申告 weight を返す。
    #[must_use]
    pub fn weight_bytes(&self) -> usize {
        self.weight_bytes
    }

    /// 判定時点で resident と外部 pin が占有していた byte 数を返す。
    #[must_use]
    pub fn live_bytes(&self) -> usize {
        self.live_bytes
    }

    /// 判定に使った cache の byte limit を返す。
    #[must_use]
    pub fn limit_bytes(&self) -> usize {
        self.limit_bytes
    }

    /// 拒否された key、value、weight を ownership ごと返す。
    #[must_use]
    pub fn into_entry(self) -> (K, V, usize) {
        (self.key, self.value, self.weight_bytes)
    }
}

impl<K, V> fmt::Display for CacheInsertError<K, V> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            CacheInsertRejection::EntryLargerThanLimit => write!(
                formatter,
                "cache entry weighs {} bytes, exceeding the {}-byte limit",
                self.weight_bytes, self.limit_bytes
            ),
            CacheInsertRejection::LiveBudgetExhausted => write!(
                formatter,
                "cache entry weighs {} bytes, but {} of {} bytes remain live",
                self.weight_bytes, self.live_bytes, self.limit_bytes
            ),
        }
    }
}

impl<K, V> Error for CacheInsertError<K, V>
where
    K: fmt::Debug,
    V: fmt::Debug,
{
}

/// 成功した insert が cache の resident set に与えた変化。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheInsertReport {
    replaced: bool,
    evicted: usize,
}

impl CacheInsertReport {
    /// 同じ key の旧 value を置き換えたかを返す。
    #[must_use]
    pub fn replaced(self) -> bool {
        self.replaced
    }

    /// 置換対象を除き、byte budget の確保のため LRU 末尾から除去した entry 数を返す。
    ///
    /// Pin 済みの entry は除去しても live byte を直ちに解放しないが、LRU victim として数える。
    #[must_use]
    pub fn evicted(self) -> usize {
        self.evicted
    }
}

/// Cache の resident/live byte accounting snapshot。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheUsage {
    /// Cache 内から key で参照できる entry の weight 合計。
    pub resident_bytes: usize,
    /// Resident と、evict 後も外部 handle が保持する entry の weight 合計。
    pub live_bytes: usize,
    /// Insert が超えない byte 上限。
    pub limit_bytes: usize,
}

/// Cache entry を lock の外で保持する read-only handle。
///
/// Clone は value を複製せず同じ allocation を pin する。Cache から entry が evict されても、最後の
/// handle が drop されるまでその weight は [`WeightedLruCache::live_bytes`] に残る。
pub struct CacheHandle<V> {
    entry: Arc<TrackedValue<V>>,
}

impl<V> CacheHandle<V> {
    /// この value が live-byte budget へ課す weight を返す。
    #[must_use]
    pub fn weight_bytes(&self) -> usize {
        self.entry.weight_bytes()
    }
}

impl<V> Clone for CacheHandle<V> {
    fn clone(&self) -> Self {
        Self {
            entry: Arc::clone(&self.entry),
        }
    }
}

impl<V> Deref for CacheHandle<V> {
    type Target = V;

    fn deref(&self) -> &Self::Target {
        &self.entry.value
    }
}

impl<V> AsRef<V> for CacheHandle<V> {
    fn as_ref(&self) -> &V {
        self
    }
}

impl<V> fmt::Debug for CacheHandle<V>
where
    V: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CacheHandle")
            .field("value", &self.entry.value)
            .field("weight_bytes", &self.entry.weight_bytes())
            .finish()
    }
}

/// Byte weight で容量を制御する deterministic LRU cache。
///
/// `get` が返す handle は value を cache lock から切り離し、`get` 自体は対象を MRU へ移す。Insert は
/// LRU 末尾から victim を選び、外部 pin のない victim が解放する byte と置換対象が解放する byte の
/// 合計で新規 weight を収容する。外部 pin だけで budget が埋まる場合は cache を変更せず拒否する。
///
/// `K` と `V` に size 計測 trait を要求しないため、syntax/wrap/intraline を同じ enum にまとめた
/// heterogeneous key/value も格納できる。後から `Mutex<WeightedLruCache<K, V>>` で共有する場合も、
/// handle の drop は cache lock を取得しない。
pub struct WeightedLruCache<K, V>
where
    K: Hash + Eq,
{
    entries: LruCache<K, Arc<TrackedValue<V>>>,
    accounting: Arc<LiveByteAccounting>,
    resident_bytes: usize,
    limit_bytes: usize,
}

impl<K, V> WeightedLruCache<K, V>
where
    K: Hash + Eq,
{
    /// 指定した byte limit を持つ空 cache を作る。
    ///
    /// Limit 0でも weight 0の metadata entry は格納できる。正の weight は
    /// [`CacheInsertRejection::EntryLargerThanLimit`] で拒否する。
    #[must_use]
    pub fn new(limit_bytes: usize) -> Self {
        Self {
            entries: LruCache::unbounded(),
            accounting: Arc::new(LiveByteAccounting::default()),
            resident_bytes: 0,
            limit_bytes,
        }
    }

    /// Cache 内から key で参照できる entry 数を返す。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Cache 内に resident entry がないかを返す。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Configured byte limit を返す。
    #[must_use]
    pub fn limit_bytes(&self) -> usize {
        self.limit_bytes
    }

    /// Cache 内から key で参照できる entry の weight 合計を返す。
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    /// Resident entry と evict 済み external pin の weight 合計を返す。
    ///
    /// Handle は別 thread で drop できるため、連続する呼び出しの間に値が減ることがある。
    #[must_use]
    pub fn live_bytes(&self) -> usize {
        self.accounting.load()
    }

    /// 現在の byte accounting を1つの snapshot として返す。
    #[must_use]
    pub fn usage(&self) -> CacheUsage {
        CacheUsage {
            resident_bytes: self.resident_bytes,
            live_bytes: self.live_bytes(),
            limit_bytes: self.limit_bytes,
        }
    }

    /// Key が resident かを LRU 順序を変えずに返す。
    #[must_use]
    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.entries.contains(key)
    }

    /// Key の value を pin して返し、対象 entry を MRU へ移す。
    pub fn get<Q>(&mut self, key: &Q) -> Option<CacheHandle<V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.entries.get(key).map(|entry| CacheHandle {
            entry: Arc::clone(entry),
        })
    }

    /// LRU 順序を変えずに key の value を pin して返す。
    pub fn peek<Q>(&self, key: &Q) -> Option<CacheHandle<V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.entries.peek(key).map(|entry| CacheHandle {
            entry: Arc::clone(entry),
        })
    }

    /// Key/value を cache へ入れ、必要なら LRU entry を追い出す。
    ///
    /// Weight が limit より大きい場合、または external pin を含む live byte に新規 weight を足せない
    /// 場合は error を返す。Error path では既存 entry と LRU 順序を変更しない。成功時の live byte は
    /// limit 以下であり、同じ key の旧 value が pin されていれば旧・新双方の weight を計上する。
    pub fn insert(
        &mut self,
        key: K,
        value: V,
        weight_bytes: usize,
    ) -> Result<CacheInsertReport, CacheInsertError<K, V>> {
        let live_bytes = self.live_bytes();
        if weight_bytes > self.limit_bytes {
            return Err(self.insert_error(
                CacheInsertRejection::EntryLargerThanLimit,
                key,
                value,
                weight_bytes,
                live_bytes,
            ));
        }

        let required_reclaim = live_bytes.saturating_sub(self.limit_bytes - weight_bytes);
        let (replacing, replacement_reclaim) =
            self.entries.peek(&key).map_or((false, 0), |entry| {
                let reclaim = if Arc::strong_count(entry) == 1 {
                    entry.weight_bytes()
                } else {
                    0
                };
                (true, reclaim)
            });

        let mut reclaimable_bytes = replacement_reclaim;
        let mut eviction_count = 0;
        if reclaimable_bytes < required_reclaim {
            for (candidate_key, entry) in self.entries.iter().rev() {
                if candidate_key == &key {
                    continue;
                }
                eviction_count += 1;
                if Arc::strong_count(entry) == 1 {
                    reclaimable_bytes = reclaimable_bytes.saturating_add(entry.weight_bytes());
                }
                if reclaimable_bytes >= required_reclaim {
                    break;
                }
            }
        }

        if reclaimable_bytes < required_reclaim {
            return Err(self.insert_error(
                CacheInsertRejection::LiveBudgetExhausted,
                key,
                value,
                weight_bytes,
                live_bytes,
            ));
        }

        if let Some(previous) = self.entries.pop(&key) {
            self.resident_bytes -= previous.weight_bytes();
            drop(previous);
        }
        for _ in 0..eviction_count {
            if let Some((_evicted_key, evicted)) = self.entries.pop_lru() {
                self.resident_bytes -= evicted.weight_bytes();
                drop(evicted);
            }
        }

        // `required_reclaim` は pin されていない Arc だけを数える。Strong count 1 の Arc は
        // preflight 後に外部から clone できず、external handle の同時 drop は空きを増やすだけなので、
        // victim drop 後の charge はこの limit 内に収まる。
        self.accounting.charge(weight_bytes, self.limit_bytes);

        let entry = Arc::new(TrackedValue {
            value,
            charge: LiveByteCharge {
                weight_bytes,
                accounting: Arc::clone(&self.accounting),
            },
        });
        let unexpected_previous = self.entries.put(key, entry);
        if let Some(previous) = unexpected_previous {
            // `pop(&key)` と `put(key, ...)` の間は `&mut self` が key set を独占する。Hash/Eq の
            // contract に反する key 実装でも accounting を二重計上しないため、返された値は処分する。
            self.resident_bytes -= previous.weight_bytes();
            drop(previous);
        }
        self.resident_bytes += weight_bytes;

        Ok(CacheInsertReport {
            replaced: replacing,
            evicted: eviction_count,
        })
    }

    /// Key を resident set から除去し、value を handle として返す。
    ///
    /// Returned handle が weight を引き継ぐため、resident byte は直ちに減るが live byte は handle の
    /// drop まで減らない。
    pub fn remove<Q>(&mut self, key: &Q) -> Option<CacheHandle<V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.entries.pop(key).map(|entry| {
            self.resident_bytes -= entry.weight_bytes();
            CacheHandle { entry }
        })
    }

    /// Resident entry をすべて除去する。
    ///
    /// 外部 handle が pin する value は解放せず、その weight は live byte に残す。
    pub fn clear(&mut self) {
        while let Some((_key, entry)) = self.entries.pop_lru() {
            self.resident_bytes -= entry.weight_bytes();
            drop(entry);
        }
    }

    fn insert_error(
        &self,
        reason: CacheInsertRejection,
        key: K,
        value: V,
        weight_bytes: usize,
        live_bytes: usize,
    ) -> CacheInsertError<K, V> {
        CacheInsertError {
            reason,
            key,
            value,
            weight_bytes,
            live_bytes,
            limit_bytes: self.limit_bytes,
        }
    }
}

impl<K, V> fmt::Debug for WeightedLruCache<K, V>
where
    K: Hash + Eq + fmt::Debug,
    V: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WeightedLruCache")
            .field("entries", &self.entries)
            .field("resident_bytes", &self.resident_bytes)
            .field("live_bytes", &self.live_bytes())
            .field("limit_bytes", &self.limit_bytes)
            .finish()
    }
}

#[derive(Debug, Default)]
struct LiveByteAccounting {
    bytes: AtomicUsize,
}

impl LiveByteAccounting {
    fn load(&self) -> usize {
        self.bytes.load(Ordering::Acquire)
    }

    fn charge(&self, weight_bytes: usize, limit_bytes: usize) {
        let mut current = self.load();
        loop {
            let Some(next) = current.checked_add(weight_bytes) else {
                panic!("cache live-byte accounting overflow");
            };
            if next > limit_bytes {
                panic!("preflighted cache charge exceeded the byte limit");
            }
            match self.bytes.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    fn release(&self, weight_bytes: usize) {
        let mut current = self.load();
        loop {
            let Some(next) = current.checked_sub(weight_bytes) else {
                debug_assert!(false, "cache live-byte accounting underflow");
                return;
            };
            match self.bytes.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }
}

struct TrackedValue<V> {
    // Rust は struct field を宣言順に drop する。Value の destructor が返る前に budget を再利用すると、
    // 最終 handle の drop と新規 insert が並行した際に実 memory が limit を超えるため、charge は
    // value より後へ置く。
    value: V,
    charge: LiveByteCharge,
}

impl<V> TrackedValue<V> {
    fn weight_bytes(&self) -> usize {
        self.charge.weight_bytes
    }
}

struct LiveByteCharge {
    weight_bytes: usize,
    accounting: Arc<LiveByteAccounting>,
}

impl Drop for LiveByteCharge {
    fn drop(&mut self) {
        self.accounting.release(self.weight_bytes);
    }
}

impl<V> fmt::Debug for TrackedValue<V>
where
    V: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TrackedValue")
            .field("value", &self.value)
            .field("weight_bytes", &self.weight_bytes())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    use super::{CacheInsertRejection, WeightedLruCache};

    #[test]
    fn get_touches_entry_before_weighted_eviction() {
        let mut cache = WeightedLruCache::new(6);
        assert!(cache.insert("a", 1, 2).is_ok());
        assert!(cache.insert("b", 2, 2).is_ok());
        assert!(cache.insert("c", 3, 2).is_ok());

        let touched = cache.get(&"a").unwrap();
        assert_eq!(*touched, 1);
        assert_eq!(touched.weight_bytes(), 2);
        drop(touched);
        let report = cache.insert("d", 4, 2).unwrap();

        assert_eq!(report.evicted(), 1);
        assert!(!report.replaced());
        assert!(cache.contains(&"a"));
        assert!(!cache.contains(&"b"));
        assert!(cache.contains(&"c"));
        assert!(cache.contains(&"d"));
        assert_eq!(cache.usage().resident_bytes, 6);
        assert_eq!(cache.usage().live_bytes, 6);
    }

    #[test]
    fn replacement_reclaims_old_weight_exactly() {
        let mut cache = WeightedLruCache::new(10);
        assert!(cache.insert("a", "old-a", 3).is_ok());
        assert!(cache.insert("b", "old-b", 3).is_ok());

        let report = cache.insert("a", "new-a", 5).unwrap();
        assert!(report.replaced());
        assert_eq!(report.evicted(), 0);
        assert_eq!(cache.resident_bytes(), 8);
        assert_eq!(cache.live_bytes(), 8);
        assert_eq!(cache.peek(&"a").as_deref(), Some(&"new-a"));

        let report = cache.insert("b", "new-b", 8).unwrap();
        assert!(report.replaced());
        assert_eq!(report.evicted(), 1);
        assert!(!cache.contains(&"a"));
        assert_eq!(cache.resident_bytes(), 8);
        assert_eq!(cache.live_bytes(), 8);
    }

    #[test]
    fn pinned_replacement_charges_old_and_new_until_release() {
        let mut cache = WeightedLruCache::new(10);
        assert!(cache.insert("a", "old", 4).is_ok());
        let old = cache.get(&"a").unwrap();

        let report = cache.insert("a", "new", 4).unwrap();
        assert!(report.replaced());
        assert_eq!(cache.resident_bytes(), 4);
        assert_eq!(cache.live_bytes(), 8);
        assert_eq!(*old, "old");
        assert_eq!(cache.peek(&"a").as_deref(), Some(&"new"));

        drop(old);
        assert_eq!(cache.live_bytes(), 4);
    }

    #[test]
    fn oversized_insert_is_rejected_without_mutating_cache() {
        let mut cache = WeightedLruCache::new(3);
        assert!(cache.insert("resident", 1, 3).is_ok());

        let error = cache.insert("large", 2, 4).unwrap_err();
        assert_eq!(error.reason(), CacheInsertRejection::EntryLargerThanLimit);
        assert_eq!(error.weight_bytes(), 4);
        assert_eq!(error.live_bytes(), 3);
        assert_eq!(error.limit_bytes(), 3);
        assert_eq!(error.into_entry(), ("large", 2, 4));
        assert!(cache.contains(&"resident"));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.live_bytes(), 3);
    }

    #[test]
    fn pinned_lru_stays_charged_after_eviction() {
        let mut cache = WeightedLruCache::new(10);
        assert!(cache.insert("pinned", "a", 3).is_ok());
        assert!(cache.insert("reclaimable", "b", 4).is_ok());
        let pinned = cache.get(&"pinned").unwrap();
        drop(cache.get(&"reclaimable"));

        let report = cache.insert("new", "c", 7).unwrap();
        assert_eq!(report.evicted(), 2);
        assert!(!cache.contains(&"pinned"));
        assert!(!cache.contains(&"reclaimable"));
        assert!(cache.contains(&"new"));
        assert_eq!(cache.resident_bytes(), 7);
        assert_eq!(cache.live_bytes(), 10);

        let second_pin = pinned.clone();
        drop(pinned);
        assert_eq!(cache.live_bytes(), 10);
        drop(second_pin);
        assert_eq!(cache.live_bytes(), 7);
    }

    #[test]
    fn pinned_budget_rejection_is_transactional() {
        let mut cache = WeightedLruCache::new(3);
        assert!(cache.insert("pinned", 1, 3).is_ok());
        let pinned = cache.get(&"pinned").unwrap();

        let error = cache.insert("new", 2, 1).unwrap_err();
        assert_eq!(error.reason(), CacheInsertRejection::LiveBudgetExhausted);
        assert_eq!(error.into_entry(), ("new", 2, 1));
        assert!(cache.contains(&"pinned"));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.resident_bytes(), 3);
        assert_eq!(cache.live_bytes(), 3);

        drop(pinned);
    }

    #[test]
    fn rejected_insert_preserves_multi_entry_lru_order() {
        let mut cache = WeightedLruCache::new(6);
        assert!(cache.insert("a", 1, 2).is_ok());
        assert!(cache.insert("b", 2, 2).is_ok());
        assert!(cache.insert("c", 3, 2).is_ok());
        let pinned_a = cache.get(&"a").unwrap();
        let pinned_b = cache.get(&"b").unwrap();

        let error = cache.insert("rejected", 4, 5).unwrap_err();
        assert_eq!(error.reason(), CacheInsertRejection::LiveBudgetExhausted);
        assert_eq!(cache.len(), 3);
        let _ = error.into_entry();
        drop(pinned_a);
        drop(pinned_b);

        let report = cache.insert("d", 5, 4).unwrap();
        assert_eq!(report.evicted(), 2);
        assert!(!cache.contains(&"c"));
        assert!(!cache.contains(&"a"));
        assert!(cache.contains(&"b"));
        assert!(cache.contains(&"d"));
    }

    #[test]
    fn remove_releases_accounting_after_last_handle() {
        let mut cache = WeightedLruCache::new(8);
        assert!(cache.insert("a", vec![1, 2, 3], 4).is_ok());
        let first = cache.get(&"a").unwrap();
        let removed = cache.remove(&"a").unwrap();

        assert!(cache.is_empty());
        assert_eq!(cache.resident_bytes(), 0);
        assert_eq!(cache.live_bytes(), 4);
        drop(removed);
        assert_eq!(cache.live_bytes(), 4);
        drop(first);
        assert_eq!(cache.live_bytes(), 0);
    }

    #[test]
    fn clear_keeps_only_externally_pinned_accounting() {
        let mut cache = WeightedLruCache::new(8);
        assert!(cache.insert("a", 1, 3).is_ok());
        assert!(cache.insert("b", 2, 5).is_ok());
        let pinned = cache.peek(&"a").unwrap();

        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.resident_bytes(), 0);
        assert_eq!(cache.live_bytes(), 3);
        drop(pinned);
        assert_eq!(cache.live_bytes(), 0);
    }

    #[test]
    fn handle_can_leave_mutex_and_release_on_another_thread() {
        let cache = Arc::new(Mutex::new(WeightedLruCache::new(8)));
        let handle = {
            let mut cache = cache.lock().unwrap();
            assert!(cache.insert("a", vec![1, 2, 3], 8).is_ok());
            cache.get(&"a").unwrap()
        };
        {
            let mut cache = cache.lock().unwrap();
            cache.clear();
            assert_eq!(cache.live_bytes(), 8);
        }

        thread::spawn(move || drop(handle)).join().unwrap();

        let cache = cache.lock().unwrap();
        assert_eq!(cache.live_bytes(), 0);
    }

    #[test]
    fn budget_is_released_after_value_destructor_finishes() {
        struct BlockingDrop {
            started: Option<Arc<Barrier>>,
            release: Option<Arc<Barrier>>,
        }

        impl BlockingDrop {
            fn immediate() -> Self {
                Self {
                    started: None,
                    release: None,
                }
            }
        }

        impl Drop for BlockingDrop {
            fn drop(&mut self) {
                if let Some(started) = &self.started {
                    started.wait();
                }
                if let Some(release) = &self.release {
                    release.wait();
                }
            }
        }

        let started = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let mut cache = WeightedLruCache::new(8);
        assert!(
            cache
                .insert(
                    "old",
                    BlockingDrop {
                        started: Some(Arc::clone(&started)),
                        release: Some(Arc::clone(&release)),
                    },
                    8,
                )
                .is_ok()
        );
        let old = cache.get(&"old").unwrap();
        cache.clear();

        let drop_thread = thread::spawn(move || drop(old));
        started.wait();
        let error = cache
            .insert("new", BlockingDrop::immediate(), 8)
            .unwrap_err();
        assert_eq!(error.reason(), CacheInsertRejection::LiveBudgetExhausted);
        assert_eq!(cache.live_bytes(), 8);

        release.wait();
        drop_thread.join().unwrap();
        assert_eq!(cache.live_bytes(), 0);
        drop(error);
        assert!(cache.insert("new", BlockingDrop::immediate(), 8).is_ok());
    }

    #[test]
    fn zero_limit_accepts_only_zero_weight_entries() {
        let mut cache = WeightedLruCache::new(0);
        assert_eq!(cache.limit_bytes(), 0);
        let error = cache.insert("positive", 1, 1).unwrap_err();
        assert_eq!(error.reason(), CacheInsertRejection::EntryLargerThanLimit);
        assert!(cache.is_empty());

        assert!(cache.insert("zero-a", 2, 0).is_ok());
        assert!(cache.insert("zero-b", 3, 0).is_ok());
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.resident_bytes(), 0);
        assert_eq!(cache.live_bytes(), 0);
    }

    #[test]
    fn enum_keys_and_values_share_one_cache() {
        #[derive(Debug, Eq, Hash, PartialEq)]
        enum Key {
            Syntax(u64),
            Wrap { line: u64, width: u16 },
        }

        #[derive(Debug, Eq, PartialEq)]
        enum Value {
            Syntax(Vec<(usize, usize)>),
            Wrap(Vec<u16>),
        }

        let mut cache = WeightedLruCache::new(16);
        assert!(
            cache
                .insert(Key::Syntax(1), Value::Syntax(vec![(0, 4)]), 8)
                .is_ok()
        );
        assert!(
            cache
                .insert(Key::Wrap { line: 1, width: 80 }, Value::Wrap(vec![4]), 8,)
                .is_ok()
        );

        let value = cache.get(&Key::Wrap { line: 1, width: 80 }).unwrap();
        assert_eq!(*value, Value::Wrap(vec![4]));
    }
}
