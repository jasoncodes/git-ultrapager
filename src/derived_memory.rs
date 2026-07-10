//! Interactive session が生成する派生データの resident byte 台帳。
//!
//! Raw input と structural index は [`crate::storage`] が別の 256 MiB 上限で管理する。この module は
//! viewport layout、公開中の装飾、次に公開する装飾、priority response、装飾計算の一時領域へ固定
//! shareを割り当てる。Viewportは[`crate::viewport`]側のcache/frame accountingが24 MiB shareを
//! 強制し、interactive ledgerは残る4つのdecoration poolをleaseする。5 shareが上限まで埋まっても
//! 合計は256 MiBを超えない。
//!
//! Search、frame preload、line decoderの再利用bufferはこのledgerの対象外であり、それぞれのraw/
//! display/retained上限で制御する。したがって[`DERIVED_MEMORY_LIMIT_BYTES`]はprocess全体のresident
//! memory上限ではなく、viewport layoutとdecoration ownershipの合計上限を表す。
//!
//! [`DerivedMemoryLedger`] は session owner が1つだけ作り、`Arc<DerivedMemoryLedger>` のような外側の
//! 共有手段で worker へ渡す。Ledger 自体と [`DerivedMemoryLease`] は `Clone` を実装しない。確保済み
//! byte を複製できない形にし、lease の最終 `Drop` が同じ session の台帳へ返却する。台帳は process
//! global を使わないため、同じ process 内の複数 session は互いの空き容量へ影響しない。

use std::sync::{Arc, Mutex, MutexGuard};

use thiserror::Error;

const MIB: usize = 1024 * 1024;
const POOL_COUNT: usize = 5;

/// 派生データ5 poolの合計上限。
pub const DERIVED_MEMORY_LIMIT_BYTES: usize = 256 * MIB;
/// 折り返し済み viewport row cache の固定上限。
pub const VIEWPORT_MEMORY_BYTES: usize = 24 * MIB;
/// 現在描画へ適用している装飾 result の固定上限。
pub const APPLIED_DECORATION_MEMORY_BYTES: usize = 64 * MIB;
/// Background worker が次に公開する装飾 result の固定上限。
pub const NEXT_DECORATION_MEMORY_BYTES: usize = 64 * MIB;
/// Jump 直後の hunk を先行計算する priority result の固定上限。
pub const PRIORITY_DECORATION_MEMORY_BYTES: usize = 8 * MIB;
/// Decode、syntax highlighter、intraline diff が計算中だけ使う共有固定上限。
pub const DECORATION_WORKSPACE_MEMORY_BYTES: usize = 96 * MIB;

const _: () = assert!(
    VIEWPORT_MEMORY_BYTES
        + APPLIED_DECORATION_MEMORY_BYTES
        + NEXT_DECORATION_MEMORY_BYTES
        + PRIORITY_DECORATION_MEMORY_BYTES
        + DECORATION_WORKSPACE_MEMORY_BYTES
        == DERIVED_MEMORY_LIMIT_BYTES
);

/// 固定 partition 内の確保先。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DerivedMemoryPool {
    Viewport,
    AppliedDecoration,
    NextDecoration,
    PriorityDecoration,
    DecorationWorkspace,
}

impl DerivedMemoryPool {
    const fn index(self) -> usize {
        match self {
            Self::Viewport => 0,
            Self::AppliedDecoration => 1,
            Self::NextDecoration => 2,
            Self::PriorityDecoration => 3,
            Self::DecorationWorkspace => 4,
        }
    }

    /// この pool から同時に lease できる byte 上限を返す。
    #[must_use]
    pub const fn limit_bytes(self) -> usize {
        match self {
            Self::Viewport => VIEWPORT_MEMORY_BYTES,
            Self::AppliedDecoration => APPLIED_DECORATION_MEMORY_BYTES,
            Self::NextDecoration => NEXT_DECORATION_MEMORY_BYTES,
            Self::PriorityDecoration => PRIORITY_DECORATION_MEMORY_BYTES,
            Self::DecorationWorkspace => DECORATION_WORKSPACE_MEMORY_BYTES,
        }
    }
}

/// Lease 操作を拒否した理由。
///
/// 失敗した操作は pool 使用量と関係する lease の byte 数を変更しない。
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum DerivedMemoryError {
    #[error(
        "derived-memory {pool:?} pool cannot reserve {requested} bytes; {available} bytes remain"
    )]
    LimitExceeded {
        pool: DerivedMemoryPool,
        requested: usize,
        available: usize,
    },
    #[error("derived-memory accounting overflow while {operation}")]
    ArithmeticOverflow { operation: &'static str },
    #[error("cannot release {requested} bytes from a {leased}-byte derived-memory lease")]
    OverRelease { requested: usize, leased: usize },
    #[error("cannot transfer derived-memory bytes between different session ledgers")]
    ForeignLease,
    #[error(
        "cannot replace applied derived memory from {replacement:?} while current lease is {current:?}"
    )]
    InvalidAppliedReplacement {
        current: DerivedMemoryPool,
        replacement: DerivedMemoryPool,
    },
}

#[derive(Debug, Default)]
struct LedgerState {
    reserved: [usize; POOL_COUNT],
}

#[derive(Debug, Default)]
struct LedgerCore {
    state: Mutex<LedgerState>,
}

impl LedgerCore {
    fn lock(&self) -> MutexGuard<'_, LedgerState> {
        // `replace_applied_while_charged`だけは旧payloadの解放をこのlock内で実行する。
        // Callbackまたは内部処理がpanicしてpoisonされても、unwind中のlease dropが残量を
        // 返せるよう、最後に記録されたstateから再開する。
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// 1 interactive session の派生メモリ使用量を直列化する台帳。
///
/// `reserve`、lease の増減・分類変更、[`Self::transfer`] は1つの mutex acquisition 内で上限判定と
/// state 更新を終える。失敗時に一時的な二重計上や部分更新は外から観測されない。この型は
/// `Clone` できないが `Send + Sync` であり、session owner が外側を `Arc` で共有できる。
#[derive(Debug, Default)]
pub struct DerivedMemoryLedger {
    core: Arc<LedgerCore>,
}

impl DerivedMemoryLedger {
    /// 使用量0の固定 partition を作る。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `pool` から `bytes` を確保する。
    ///
    /// 成功時の lease は `Drop` まで byte を保持する。0 byte lease も有効で、後から
    /// [`DerivedMemoryLease::reserve`] で増やせる。
    pub fn reserve(
        &self,
        pool: DerivedMemoryPool,
        bytes: usize,
    ) -> Result<DerivedMemoryLease, DerivedMemoryError> {
        let mut state = self.core.lock();
        let index = pool.index();
        let next = checked_pool_reservation(state.reserved[index], pool, bytes)?;
        state.reserved[index] = next;
        drop(state);
        Ok(DerivedMemoryLease {
            core: Arc::clone(&self.core),
            pool,
            bytes,
        })
    }

    /// 2つの lease 間で `bytes` の所有権を移す。
    ///
    /// 両 lease がこの ledger 由来であることを検証し、移動先 pool の上限判定、pool 使用量、両 lease
    /// の byte 数を同じ critical section で更新する。同じ pool 内の移動では pool 合計を変えない。
    pub fn transfer(
        &self,
        source: &mut DerivedMemoryLease,
        destination: &mut DerivedMemoryLease,
        bytes: usize,
    ) -> Result<(), DerivedMemoryError> {
        if !Arc::ptr_eq(&self.core, &source.core) || !Arc::ptr_eq(&self.core, &destination.core) {
            return Err(DerivedMemoryError::ForeignLease);
        }
        if bytes > source.bytes {
            return Err(DerivedMemoryError::OverRelease {
                requested: bytes,
                leased: source.bytes,
            });
        }

        let next_source_lease =
            source
                .bytes
                .checked_sub(bytes)
                .ok_or(DerivedMemoryError::ArithmeticOverflow {
                    operation: "subtracting a transfer source lease",
                })?;
        let next_destination_lease =
            destination
                .bytes
                .checked_add(bytes)
                .ok_or(DerivedMemoryError::ArithmeticOverflow {
                    operation: "growing a transfer destination lease",
                })?;

        let mut state = self.core.lock();
        if source.pool == destination.pool {
            source.bytes = next_source_lease;
            destination.bytes = next_destination_lease;
            return Ok(());
        }

        let source_index = source.pool.index();
        let destination_index = destination.pool.index();
        let next_source_pool = state.reserved[source_index].checked_sub(bytes).ok_or(
            DerivedMemoryError::ArithmeticOverflow {
                operation: "subtracting a transfer source pool",
            },
        )?;
        let next_destination_pool =
            checked_pool_reservation(state.reserved[destination_index], destination.pool, bytes)?;

        state.reserved[source_index] = next_source_pool;
        state.reserved[destination_index] = next_destination_pool;
        source.bytes = next_source_lease;
        destination.bytes = next_destination_lease;
        Ok(())
    }

    /// 公開中のApplied leaseをNextまたはPriority leaseで置き換える。
    ///
    /// `current`の全byte解放、`replacement`の元poolからの減算、Appliedへの再分類を同じlock内で
    /// 反映する。Applied poolが旧resultで上限まで埋まっていても、一時的に旧+新を加算しない。
    /// 成功後は`current.bytes() == 0`、`replacement.pool() == AppliedDecoration`となる。`drop_current`
    /// は上限判定後、accounting更新前にledger lockを保持したまま1回だけ呼ぶ。旧payloadが解放される
    /// 前に別threadが空いたNext/Applied poolを予約する隙間を作らない。
    ///
    /// Callbackがpanicした場合はpool stateと両leaseを変更せずunwindする。Callbackはこのledgerを
    /// 操作してはならない。同lockを再取得するためdeadlockする。Mutex poisonはlease Dropが回収できる
    /// ため、旧payloadを所有するwrapperはfield drop順でcurrent leaseも解放する。
    pub fn replace_applied_while_charged<ReleaseCurrent>(
        &self,
        current: &mut DerivedMemoryLease,
        replacement: &mut DerivedMemoryLease,
        drop_current: ReleaseCurrent,
    ) -> Result<(), DerivedMemoryError>
    where
        ReleaseCurrent: FnOnce(),
    {
        if !Arc::ptr_eq(&self.core, &current.core) || !Arc::ptr_eq(&self.core, &replacement.core) {
            return Err(DerivedMemoryError::ForeignLease);
        }
        if current.pool != DerivedMemoryPool::AppliedDecoration
            || !matches!(
                replacement.pool,
                DerivedMemoryPool::NextDecoration | DerivedMemoryPool::PriorityDecoration
            )
        {
            return Err(DerivedMemoryError::InvalidAppliedReplacement {
                current: current.pool,
                replacement: replacement.pool,
            });
        }

        let mut state = self.core.lock();
        let applied_index = DerivedMemoryPool::AppliedDecoration.index();
        let replacement_index = replacement.pool.index();
        let applied_without_current = state.reserved[applied_index]
            .checked_sub(current.bytes)
            .ok_or(DerivedMemoryError::ArithmeticOverflow {
                operation: "releasing the previous applied lease",
            })?;
        let next_applied = checked_pool_reservation(
            applied_without_current,
            DerivedMemoryPool::AppliedDecoration,
            replacement.bytes,
        )?;
        let next_replacement_pool = state.reserved[replacement_index]
            .checked_sub(replacement.bytes)
            .ok_or(DerivedMemoryError::ArithmeticOverflow {
                operation: "reclassifying an applied replacement",
            })?;

        drop_current();
        state.reserved[applied_index] = next_applied;
        state.reserved[replacement_index] = next_replacement_pool;
        current.bytes = 0;
        replacement.pool = DerivedMemoryPool::AppliedDecoration;
        Ok(())
    }

    /// 現在の全 pool 使用量を1時点の値として返す。
    #[must_use]
    pub fn snapshot(&self) -> DerivedMemorySnapshot {
        let state = self.core.lock();
        DerivedMemorySnapshot {
            reserved: state.reserved,
        }
    }
}

/// 1回の計算または1つの resident object が占有する byte の所有権。
///
/// Lease は複製できない。別 thread へ move した場合も同じ session ledger を参照し、明示的な
/// [`Self::release`] または `Drop` で返却する。格納する object を破棄する前に lease だけを release
/// すると未計上の resident memory が残るため、owner struct では object より後に drop される field
/// 順序にする。
#[derive(Debug)]
pub struct DerivedMemoryLease {
    core: Arc<LedgerCore>,
    pool: DerivedMemoryPool,
    bytes: usize,
}

impl DerivedMemoryLease {
    /// この lease が属する pool を返す。
    #[must_use]
    pub fn pool(&self) -> DerivedMemoryPool {
        self.pool
    }

    /// この lease が計上している byte 数を返す。
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// 同じ pool から `bytes` を追加確保する。
    ///
    /// 上限または `usize` を超える場合、lease と ledger は変更しない。
    pub fn reserve(&mut self, bytes: usize) -> Result<(), DerivedMemoryError> {
        let next_lease =
            self.bytes
                .checked_add(bytes)
                .ok_or(DerivedMemoryError::ArithmeticOverflow {
                    operation: "growing a lease",
                })?;
        let mut state = self.core.lock();
        let index = self.pool.index();
        let next_pool = checked_pool_reservation(state.reserved[index], self.pool, bytes)?;
        state.reserved[index] = next_pool;
        self.bytes = next_lease;
        Ok(())
    }

    /// この lease から `bytes` を返却する。
    ///
    /// Lease の残量を超える要求は [`DerivedMemoryError::OverRelease`] となり、0まで丸めない。
    pub fn release(&mut self, bytes: usize) -> Result<(), DerivedMemoryError> {
        let next_lease = self
            .bytes
            .checked_sub(bytes)
            .ok_or(DerivedMemoryError::OverRelease {
                requested: bytes,
                leased: self.bytes,
            })?;
        let mut state = self.core.lock();
        let index = self.pool.index();
        let next_pool = state.reserved[index].checked_sub(bytes).ok_or(
            DerivedMemoryError::ArithmeticOverflow {
                operation: "releasing a pool reservation",
            },
        )?;
        state.reserved[index] = next_pool;
        self.bytes = next_lease;
        Ok(())
    }

    /// Lease 全体を別 pool へ分類変更する。
    ///
    /// 移動先に全 byte が収まる場合だけ、移動元の減算と移動先の加算を同時に反映する。Result object
    /// の workspace 所有権を resident result へ引き継ぐような、二重計上を挟めない handoff に使う。
    pub fn reclassify(&mut self, pool: DerivedMemoryPool) -> Result<(), DerivedMemoryError> {
        if self.pool == pool {
            return Ok(());
        }
        let mut state = self.core.lock();
        let source_index = self.pool.index();
        let destination_index = pool.index();
        let next_source = state.reserved[source_index].checked_sub(self.bytes).ok_or(
            DerivedMemoryError::ArithmeticOverflow {
                operation: "reclassifying a source pool",
            },
        )?;
        let next_destination =
            checked_pool_reservation(state.reserved[destination_index], pool, self.bytes)?;

        state.reserved[source_index] = next_source;
        state.reserved[destination_index] = next_destination;
        self.pool = pool;
        Ok(())
    }
}

impl Drop for DerivedMemoryLease {
    fn drop(&mut self) {
        let mut state = self.core.lock();
        let index = self.pool.index();
        // `bytes` と pool 使用量は private API の同じ critical section だけで更新する。Unwind 中に
        // 二重panicを起こさないため、破損を検出した場合もDropからpanicせずpoolを空へ戻す。
        let Some(next) = state.reserved[index].checked_sub(self.bytes) else {
            state.reserved[index] = 0;
            self.bytes = 0;
            return;
        };
        state.reserved[index] = next;
        self.bytes = 0;
    }
}

/// [`DerivedMemoryLedger::snapshot`] が返す読み取り専用 usage。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DerivedMemorySnapshot {
    reserved: [usize; POOL_COUNT],
}

impl DerivedMemorySnapshot {
    /// 指定 pool の確保済み byte 数を返す。
    #[must_use]
    pub fn reserved_bytes(self, pool: DerivedMemoryPool) -> usize {
        self.reserved[pool.index()]
    }

    /// 5 pool の確保済み byte 合計をchecked加算で返す。
    ///
    /// 各 pool は固定上限以下なので通常は `Some` になる。返り値を `Option` にして、新しい pool を
    /// 追加した際に合計式だけが暗黙にwrapする形を避ける。
    #[must_use]
    pub fn total_reserved_bytes(self) -> Option<usize> {
        self.reserved
            .into_iter()
            .try_fold(0_usize, usize::checked_add)
    }
}

fn checked_pool_reservation(
    current: usize,
    pool: DerivedMemoryPool,
    requested: usize,
) -> Result<usize, DerivedMemoryError> {
    let next = current
        .checked_add(requested)
        .ok_or(DerivedMemoryError::ArithmeticOverflow {
            operation: "growing a pool reservation",
        })?;
    let limit = pool.limit_bytes();
    if next > limit {
        return Err(DerivedMemoryError::LimitExceeded {
            pool,
            requested,
            available: limit.saturating_sub(current),
        });
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn fixed_partition_is_exactly_256_mib() {
        let pools = [
            DerivedMemoryPool::Viewport,
            DerivedMemoryPool::AppliedDecoration,
            DerivedMemoryPool::NextDecoration,
            DerivedMemoryPool::PriorityDecoration,
            DerivedMemoryPool::DecorationWorkspace,
        ];
        assert_eq!(
            pools
                .into_iter()
                .map(DerivedMemoryPool::limit_bytes)
                .sum::<usize>(),
            DERIVED_MEMORY_LIMIT_BYTES
        );
        assert_eq!(DERIVED_MEMORY_LIMIT_BYTES, 256 * 1024 * 1024);
        assert_eq!(VIEWPORT_MEMORY_BYTES, 24 * MIB);
        assert_eq!(APPLIED_DECORATION_MEMORY_BYTES, 64 * MIB);
        assert_eq!(NEXT_DECORATION_MEMORY_BYTES, 64 * MIB);
        assert_eq!(PRIORITY_DECORATION_MEMORY_BYTES, 8 * MIB);
        assert_eq!(DECORATION_WORKSPACE_MEMORY_BYTES, 96 * MIB);
    }

    #[test]
    fn exact_pool_limit_and_checked_release_are_deterministic() {
        let ledger = DerivedMemoryLedger::new();
        let mut lease = ledger
            .reserve(
                DerivedMemoryPool::PriorityDecoration,
                PRIORITY_DECORATION_MEMORY_BYTES,
            )
            .unwrap();
        assert_eq!(
            ledger
                .reserve(DerivedMemoryPool::PriorityDecoration, 1)
                .unwrap_err(),
            DerivedMemoryError::LimitExceeded {
                pool: DerivedMemoryPool::PriorityDecoration,
                requested: 1,
                available: 0,
            }
        );
        assert_eq!(
            lease.release(PRIORITY_DECORATION_MEMORY_BYTES + 1),
            Err(DerivedMemoryError::OverRelease {
                requested: PRIORITY_DECORATION_MEMORY_BYTES + 1,
                leased: PRIORITY_DECORATION_MEMORY_BYTES,
            })
        );
        lease.release(1).unwrap();
        let one_byte = ledger
            .reserve(DerivedMemoryPool::PriorityDecoration, 1)
            .unwrap();
        assert_eq!(
            ledger
                .snapshot()
                .reserved_bytes(DerivedMemoryPool::PriorityDecoration),
            PRIORITY_DECORATION_MEMORY_BYTES
        );
        drop(one_byte);
        drop(lease);
        assert_eq!(ledger.snapshot().total_reserved_bytes(), Some(0));
    }

    #[test]
    fn reserve_and_reclassify_fail_without_partial_updates() {
        let ledger = DerivedMemoryLedger::new();
        let _priority = ledger
            .reserve(
                DerivedMemoryPool::PriorityDecoration,
                PRIORITY_DECORATION_MEMORY_BYTES,
            )
            .unwrap();
        let mut applied = ledger
            .reserve(DerivedMemoryPool::AppliedDecoration, 17)
            .unwrap();
        let before = ledger.snapshot();
        assert_eq!(
            applied.reclassify(DerivedMemoryPool::PriorityDecoration),
            Err(DerivedMemoryError::LimitExceeded {
                pool: DerivedMemoryPool::PriorityDecoration,
                requested: 17,
                available: 0,
            })
        );
        assert_eq!(applied.pool(), DerivedMemoryPool::AppliedDecoration);
        assert_eq!(applied.bytes(), 17);
        assert_eq!(ledger.snapshot(), before);

        applied
            .reclassify(DerivedMemoryPool::NextDecoration)
            .unwrap();
        assert_eq!(applied.pool(), DerivedMemoryPool::NextDecoration);
        assert_eq!(
            ledger
                .snapshot()
                .reserved_bytes(DerivedMemoryPool::AppliedDecoration),
            0
        );
    }

    #[test]
    fn lease_growth_overflow_is_rejected_without_changing_usage() {
        let ledger = DerivedMemoryLedger::new();
        let mut lease = ledger
            .reserve(DerivedMemoryPool::AppliedDecoration, 1)
            .unwrap();
        let before = ledger.snapshot();
        assert_eq!(
            lease.reserve(usize::MAX),
            Err(DerivedMemoryError::ArithmeticOverflow {
                operation: "growing a lease",
            })
        );
        assert_eq!(lease.bytes(), 1);
        assert_eq!(ledger.snapshot(), before);
    }

    #[test]
    fn transfer_is_atomic_across_pools_and_within_one_pool() {
        let ledger = DerivedMemoryLedger::new();
        let mut source = ledger
            .reserve(DerivedMemoryPool::AppliedDecoration, 100)
            .unwrap();
        let mut destination = ledger
            .reserve(
                DerivedMemoryPool::PriorityDecoration,
                PRIORITY_DECORATION_MEMORY_BYTES - 10,
            )
            .unwrap();
        let before = ledger.snapshot();
        assert_eq!(
            ledger.transfer(&mut source, &mut destination, 11),
            Err(DerivedMemoryError::LimitExceeded {
                pool: DerivedMemoryPool::PriorityDecoration,
                requested: 11,
                available: 10,
            })
        );
        assert_eq!(source.bytes(), 100);
        assert_eq!(destination.bytes(), PRIORITY_DECORATION_MEMORY_BYTES - 10);
        assert_eq!(ledger.snapshot(), before);

        ledger.transfer(&mut source, &mut destination, 10).unwrap();
        assert_eq!(source.bytes(), 90);
        assert_eq!(destination.bytes(), PRIORITY_DECORATION_MEMORY_BYTES);

        let mut same_pool = ledger
            .reserve(DerivedMemoryPool::AppliedDecoration, 1)
            .unwrap();
        let applied_before = ledger
            .snapshot()
            .reserved_bytes(DerivedMemoryPool::AppliedDecoration);
        ledger.transfer(&mut source, &mut same_pool, 40).unwrap();
        assert_eq!(source.bytes(), 50);
        assert_eq!(same_pool.bytes(), 41);
        assert_eq!(
            ledger
                .snapshot()
                .reserved_bytes(DerivedMemoryPool::AppliedDecoration),
            applied_before
        );
    }

    #[test]
    fn priority_replaces_a_larger_applied_lease_in_one_transition() {
        let ledger = DerivedMemoryLedger::new();
        let mut current = ledger
            .reserve(
                DerivedMemoryPool::AppliedDecoration,
                APPLIED_DECORATION_MEMORY_BYTES,
            )
            .unwrap();
        let mut priority = ledger
            .reserve(DerivedMemoryPool::PriorityDecoration, 7 * MIB)
            .unwrap();

        ledger
            .replace_applied_while_charged(&mut current, &mut priority, || {})
            .unwrap();
        assert_eq!(current.bytes(), 0);
        assert_eq!(priority.bytes(), 7 * MIB);
        assert_eq!(priority.pool(), DerivedMemoryPool::AppliedDecoration);
        let usage = ledger.snapshot();
        assert_eq!(
            usage.reserved_bytes(DerivedMemoryPool::AppliedDecoration),
            7 * MIB
        );
        assert_eq!(
            usage.reserved_bytes(DerivedMemoryPool::PriorityDecoration),
            0
        );
    }

    #[test]
    fn next_replaces_a_smaller_applied_lease_in_one_transition() {
        let ledger = DerivedMemoryLedger::new();
        let mut current = ledger
            .reserve(
                DerivedMemoryPool::AppliedDecoration,
                APPLIED_DECORATION_MEMORY_BYTES,
            )
            .unwrap();
        let mut next = ledger
            .reserve(DerivedMemoryPool::NextDecoration, 57 * MIB)
            .unwrap();

        ledger
            .replace_applied_while_charged(&mut current, &mut next, || {})
            .unwrap();
        assert_eq!(current.bytes(), 0);
        assert_eq!(next.bytes(), 57 * MIB);
        assert_eq!(next.pool(), DerivedMemoryPool::AppliedDecoration);
        let usage = ledger.snapshot();
        assert_eq!(
            usage.reserved_bytes(DerivedMemoryPool::AppliedDecoration),
            57 * MIB
        );
        assert_eq!(usage.reserved_bytes(DerivedMemoryPool::NextDecoration), 0);
    }

    #[test]
    fn invalid_applied_replacement_does_not_change_either_lease() {
        let ledger = DerivedMemoryLedger::new();
        let mut current = ledger
            .reserve(DerivedMemoryPool::NextDecoration, 2)
            .unwrap();
        let mut replacement = ledger
            .reserve(DerivedMemoryPool::PriorityDecoration, 3)
            .unwrap();
        let before = ledger.snapshot();
        let callback_called = Cell::new(false);
        assert_eq!(
            ledger.replace_applied_while_charged(&mut current, &mut replacement, || {
                callback_called.set(true);
            }),
            Err(DerivedMemoryError::InvalidAppliedReplacement {
                current: DerivedMemoryPool::NextDecoration,
                replacement: DerivedMemoryPool::PriorityDecoration,
            })
        );
        assert!(!callback_called.get());
        assert_eq!(current.bytes(), 2);
        assert_eq!(replacement.bytes(), 3);
        assert_eq!(ledger.snapshot(), before);
    }

    #[test]
    fn panicking_applied_drop_callback_leaves_accounting_unchanged() {
        let ledger = DerivedMemoryLedger::new();
        let mut current = ledger
            .reserve(DerivedMemoryPool::AppliedDecoration, 4096)
            .unwrap();
        let mut replacement = ledger
            .reserve(DerivedMemoryPool::NextDecoration, 6144)
            .unwrap();
        let before = ledger.snapshot();

        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let _ = ledger.replace_applied_while_charged(&mut current, &mut replacement, || {
                panic!("synthetic payload drop panic")
            });
        }));
        assert!(outcome.is_err());
        assert_eq!(current.pool(), DerivedMemoryPool::AppliedDecoration);
        assert_eq!(current.bytes(), 4096);
        assert_eq!(replacement.pool(), DerivedMemoryPool::NextDecoration);
        assert_eq!(replacement.bytes(), 6144);
        assert_eq!(ledger.snapshot(), before);

        drop(replacement);
        drop(current);
        assert_eq!(ledger.snapshot().total_reserved_bytes(), Some(0));
    }

    #[test]
    fn transfer_rejects_foreign_session_leases() {
        let first = DerivedMemoryLedger::new();
        let second = DerivedMemoryLedger::new();
        let mut source = first
            .reserve(DerivedMemoryPool::AppliedDecoration, 1)
            .unwrap();
        let mut destination = second
            .reserve(DerivedMemoryPool::AppliedDecoration, 1)
            .unwrap();
        assert_eq!(
            first.transfer(&mut source, &mut destination, 1),
            Err(DerivedMemoryError::ForeignLease)
        );
        assert_eq!(source.bytes(), 1);
        assert_eq!(destination.bytes(), 1);
    }

    #[test]
    fn lease_drop_during_panic_releases_the_pool() {
        let ledger = DerivedMemoryLedger::new();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let _lease = ledger
                .reserve(DerivedMemoryPool::DecorationWorkspace, 4096)
                .unwrap();
            panic!("synthetic worker panic");
        }));
        assert!(outcome.is_err());
        assert_eq!(
            ledger
                .snapshot()
                .reserved_bytes(DerivedMemoryPool::DecorationWorkspace),
            0
        );
    }

    #[test]
    fn resident_payload_drops_before_its_lease_field() {
        struct PayloadProbe<'a> {
            ledger: &'a DerivedMemoryLedger,
            dropped_while_charged: Arc<AtomicBool>,
        }

        impl Drop for PayloadProbe<'_> {
            fn drop(&mut self) {
                let charged = self
                    .ledger
                    .snapshot()
                    .reserved_bytes(DerivedMemoryPool::NextDecoration)
                    == 4096;
                self.dropped_while_charged.store(charged, Ordering::SeqCst);
            }
        }

        // Rustはfield宣言順にdropする。Resident ownerはpayloadをleaseより前へ宣言し、payloadの
        // destructorが返るまでresident byteを台帳に残す。
        struct Resident<'a> {
            _payload: PayloadProbe<'a>,
            _lease: DerivedMemoryLease,
        }

        let ledger = DerivedMemoryLedger::new();
        let dropped_while_charged = Arc::new(AtomicBool::new(false));
        let resident = Resident {
            _payload: PayloadProbe {
                ledger: &ledger,
                dropped_while_charged: Arc::clone(&dropped_while_charged),
            },
            _lease: ledger
                .reserve(DerivedMemoryPool::NextDecoration, 4096)
                .unwrap(),
        };
        drop(resident);
        assert!(dropped_while_charged.load(Ordering::SeqCst));
        assert_eq!(ledger.snapshot().total_reserved_bytes(), Some(0));
    }

    #[test]
    fn payload_destructor_panic_still_drops_the_following_lease() {
        struct PanickingPayload;

        impl Drop for PanickingPayload {
            fn drop(&mut self) {
                panic!("synthetic payload destructor panic");
            }
        }

        struct Resident {
            _payload: PanickingPayload,
            _lease: DerivedMemoryLease,
        }

        let ledger = DerivedMemoryLedger::new();
        let resident = Resident {
            _payload: PanickingPayload,
            _lease: ledger
                .reserve(DerivedMemoryPool::AppliedDecoration, 2048)
                .unwrap(),
        };
        let outcome = catch_unwind(AssertUnwindSafe(|| drop(resident)));
        assert!(outcome.is_err());
        assert_eq!(ledger.snapshot().total_reserved_bytes(), Some(0));
    }

    #[test]
    fn shared_reference_is_thread_safe_without_cloning_the_ledger() {
        let ledger = DerivedMemoryLedger::new();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    let mut lease = ledger.reserve(DerivedMemoryPool::Viewport, 1024).unwrap();
                    lease.reserve(1024).unwrap();
                    lease.release(512).unwrap();
                });
            }
        });
        assert_eq!(ledger.snapshot().total_reserved_bytes(), Some(0));
    }
}
