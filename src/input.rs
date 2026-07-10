//! stdin reader、memory tier、temporary spill store。
//!
//! `AppendStore` は受信済み byte の完全な copy を所有する。64 MiBまでは memory に保持し、次の
//! append が上限を超える直前に既存内容を permission 0600 の unlinked tempfile へ移す。parser
//! metadata や derived cache はこの module の budget に含めない。ReaderがEOFを得たら[`AppendStore::seal`]
//! がhigh-water markを固定し、同じstoreを共有する全cloneの後続appendを拒否する。

use std::fs::File;
use std::io;
use std::sync::{Arc, RwLock};

use thiserror::Error;

use crate::storage::{read_exact_at, secure_tempfile, write_all_at};

/// Raw input を memory に置く最大 byte 数。
pub const MEMORY_TIER_BYTES: usize = 64 * 1024 * 1024;

/// Append-only store の failure。
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("input store lock was poisoned")]
    LockPoisoned,
    #[error("input store I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("could not allocate {requested} bytes for an input range")]
    Allocation { requested: usize },
    #[error("requested range {start}..{end} exceeds stored length {len}")]
    OutOfBounds { start: u64, end: u64, len: u64 },
    #[error("input store was sealed at byte {len}; no further append is allowed")]
    Sealed { len: u64 },
}

/// Reader thread と background worker が共有できる、EOF seal付きappend-only byte store。
#[derive(Clone, Debug)]
pub struct AppendStore {
    inner: Arc<RwLock<StoreState>>,
    memory_limit: usize,
}

#[derive(Debug)]
struct StoreState {
    /// Memory/temporary-file tierと、append済みlogical length。
    backing: StoreBacking,
    /// `true`になった後はbackingとlengthを変更しない。
    sealed: bool,
}

#[derive(Debug)]
enum StoreBacking {
    Memory(Vec<u8>),
    Spill { file: Arc<File>, len: u64 },
}

/// Raw storeの長さとEOF sealを同じlock acquisitionで読んだ状態。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AppendStoreStatus {
    /// Status取得時のappend済みbyte数。Sealedなら以後変化しない。
    pub len: u64,
    /// EOF high-water markが固定済みなら`true`。
    pub sealed: bool,
}

impl AppendStore {
    /// 仕様既定の64 MiB memory tierを持つ空 store を作る。
    #[must_use]
    pub fn new() -> Self {
        Self::with_memory_limit(MEMORY_TIER_BYTES)
    }

    /// Test と constrained environment 用に memory tierを指定する。
    #[must_use]
    pub fn with_memory_limit(memory_limit: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(StoreState {
                backing: StoreBacking::Memory(Vec::new()),
                sealed: false,
            })),
            memory_limit,
        }
    }

    /// Byte を末尾へ追加し、追加前の absolute offset を返す。Seal後は空sliceも拒否する。
    pub fn append(&self, bytes: &[u8]) -> Result<u64, StoreError> {
        let mut state = self.inner.write().map_err(|_| StoreError::LockPoisoned)?;
        let start = state.backing.len();
        if state.sealed {
            return Err(StoreError::Sealed { len: start });
        }
        let added = u64::try_from(bytes.len())
            .map_err(|_| io::Error::other("append length does not fit input offsets"))?;
        let end = start
            .checked_add(added)
            .ok_or_else(|| io::Error::other("input length overflow"))?;

        if let StoreBacking::Memory(memory) = &mut state.backing {
            let exceeds_limit = memory
                .len()
                .checked_add(bytes.len())
                .is_none_or(|length| length > self.memory_limit);
            if !exceeds_limit {
                // `extend_from_slice` の暗黙 allocation failure は process abort になり得る。reserveを
                // fallible API で先に済ませると、store failure として terminal lifecycleへ返せる。
                memory
                    .try_reserve(bytes.len())
                    .map_err(|_| StoreError::Allocation {
                        requested: bytes.len(),
                    })?;
                memory.extend_from_slice(bytes);
                return Ok(start);
            }

            let file = Arc::new(secure_tempfile()?);
            write_all_at(&file, memory, 0)?;
            write_all_at(&file, bytes, start)?;
            state.backing = StoreBacking::Spill { file, len: end };
            return Ok(start);
        }

        if let StoreBacking::Spill { file, len } = &mut state.backing {
            // 直前のpositioned writeが途中で失敗した場合も、logical lengthから上書きすれば、
            // 再試行で未commit byteを入力の一部として誤採用しない。
            write_all_at(file, bytes, *len)?;
            *len = end;
        }
        Ok(start)
    }

    /// EOF high-water markを固定し、以後のappendを全cloneから拒否する。
    ///
    /// Appendと同じwrite lock内で`sealed`を更新するため、競合時はappend全体が先にcommitして返却値へ
    /// 含まれるか、sealが先に成立してappendが[`StoreError::Sealed`]になるかのどちらかとなる。Repeated
    /// callは同じhigh-water markを返す。
    pub fn seal(&self) -> Result<u64, StoreError> {
        let mut state = self.inner.write().map_err(|_| StoreError::LockPoisoned)?;
        let len = state.backing.len();
        state.sealed = true;
        Ok(len)
    }

    /// Absolute byte range をcopyして返す。
    pub fn read_range(&self, range: std::ops::Range<u64>) -> Result<Vec<u8>, StoreError> {
        let mut output = Vec::new();
        self.read_range_into(range, &mut output)?;
        Ok(output)
    }

    /// Absolute byte range をcallerのbufferへcopyする。
    ///
    /// 既存capacityを再利用し、内容は指定rangeだけへ置き換える。Spill後はfile cursorを共有しない
    /// positioned readを使うため、複数のsearch/layout workerがrangeごとに並行して読める。
    pub fn read_range_into(
        &self,
        range: std::ops::Range<u64>,
        output: &mut Vec<u8>,
    ) -> Result<(), StoreError> {
        let state = self.inner.read().map_err(|_| StoreError::LockPoisoned)?;
        let len = state.backing.len();
        if range.start > range.end || range.end > len {
            return Err(StoreError::OutOfBounds {
                start: range.start,
                end: range.end,
                len,
            });
        }
        let requested = usize::try_from(range.end - range.start)
            .map_err(|_| io::Error::other("requested range does not fit memory"))?;

        let spill_file = match &state.backing {
            StoreBacking::Memory(memory) => {
                output.clear();
                output
                    .try_reserve_exact(requested)
                    .map_err(|_| StoreError::Allocation { requested })?;
                output.resize(requested, 0);
                let start = usize::try_from(range.start)
                    .map_err(|_| io::Error::other("offset does not fit memory"))?;
                let end = start
                    .checked_add(requested)
                    .ok_or_else(|| io::Error::other("requested range overflow"))?;
                output.copy_from_slice(&memory[start..end]);
                return Ok(());
            }
            StoreBacking::Spill { file, .. } => Arc::clone(file),
        };
        // Spill fileはArcで所有し、logical lengthとdescriptor cloneを取得した後にlockを解放する。
        // Positioned read中もreader threadは別rangeへappendでき、sealもpread完了を待たない。
        drop(state);
        output.clear();
        output
            .try_reserve_exact(requested)
            .map_err(|_| StoreError::Allocation { requested })?;
        output.resize(requested, 0);
        read_exact_at(&spill_file, output, range.start)?;
        Ok(())
    }

    /// 現在受信済みの byte 数を返す。
    pub fn len(&self) -> Result<u64, StoreError> {
        self.status().map(|status| status.len)
    }

    /// Store が空かどうかを返す。
    pub fn is_empty(&self) -> Result<bool, StoreError> {
        self.len().map(|length| length == 0)
    }

    /// Tempfile へspill済みかを返す。Pathは公開しない。
    pub fn is_spilled(&self) -> Result<bool, StoreError> {
        let state = self.inner.read().map_err(|_| StoreError::LockPoisoned)?;
        Ok(matches!(&state.backing, StoreBacking::Spill { .. }))
    }

    /// EOF seal済みかを返す。Seal後の長さは変化しない。
    pub fn is_sealed(&self) -> Result<bool, StoreError> {
        self.status().map(|status| status.sealed)
    }

    /// Lengthとsealをatomic snapshotとして返す。
    pub(crate) fn status(&self) -> Result<AppendStoreStatus, StoreError> {
        let state = self.inner.read().map_err(|_| StoreError::LockPoisoned)?;
        Ok(AppendStoreStatus {
            len: state.backing.len(),
            sealed: state.sealed,
        })
    }
}

impl Default for AppendStore {
    fn default() -> Self {
        Self::new()
    }
}

impl StoreBacking {
    fn len(&self) -> u64 {
        match self {
            Self::Memory(bytes) => bytes.len() as u64,
            Self::Spill { len, .. } => *len,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;

    use super::*;

    #[test]
    fn remains_in_memory_at_limit() {
        let store = AppendStore::with_memory_limit(4);
        assert_eq!(store.append(b"abcd").unwrap(), 0);
        assert!(!store.is_spilled().unwrap());
        assert_eq!(store.read_range(1..3).unwrap(), b"bc");
    }

    #[test]
    fn spills_before_first_over_limit_append() {
        let store = AppendStore::with_memory_limit(4);
        store.append(b"abc").unwrap();
        assert_eq!(store.append(b"def").unwrap(), 3);
        assert!(store.is_spilled().unwrap());
        assert_eq!(store.read_range(0..6).unwrap(), b"abcdef");
    }

    #[test]
    fn rejects_out_of_bounds_ranges() {
        let store = AppendStore::with_memory_limit(4);
        store.append(b"abc").unwrap();
        assert!(matches!(
            store.read_range(2..4),
            Err(StoreError::OutOfBounds { .. })
        ));
    }

    #[test]
    fn read_range_into_reuses_caller_buffer_before_and_after_spill() {
        let store = AppendStore::with_memory_limit(4);
        store.append(b"abcd").unwrap();
        let mut output = Vec::with_capacity(16);
        let capacity = output.capacity();
        store.read_range_into(1..3, &mut output).unwrap();
        assert_eq!(output, b"bc");
        assert_eq!(output.capacity(), capacity);

        store.append(b"efgh").unwrap();
        store.read_range_into(2..7, &mut output).unwrap();
        assert_eq!(output, b"cdefg");
        assert_eq!(output.capacity(), capacity);
    }

    #[test]
    fn concurrent_appends_return_non_overlapping_offsets() {
        let store = AppendStore::with_memory_limit(3);
        let mut threads = Vec::new();
        for byte in b'a'..=b'h' {
            let store = store.clone();
            threads.push(std::thread::spawn(move || {
                let offset = store.append(&[byte]).unwrap();
                (offset, byte)
            }));
        }

        let mut writes: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        writes.sort_by_key(|(offset, _)| *offset);
        assert_eq!(
            writes.iter().map(|(offset, _)| *offset).collect::<Vec<_>>(),
            (0..8).collect::<Vec<_>>()
        );
        let expected: Vec<_> = writes.into_iter().map(|(_, byte)| byte).collect();
        assert_eq!(store.read_range(0..8).unwrap(), expected);
    }

    #[test]
    fn seal_is_idempotent_shared_and_rejects_even_empty_appends() {
        let store = AppendStore::with_memory_limit(1);
        store.append(b"abc").unwrap();
        let clone = store.clone();

        assert_eq!(clone.seal().unwrap(), 3);
        assert_eq!(store.seal().unwrap(), 3);
        assert!(store.is_sealed().unwrap());
        assert!(clone.is_sealed().unwrap());
        assert!(matches!(
            store.append(b"d"),
            Err(StoreError::Sealed { len: 3 })
        ));
        assert!(matches!(
            clone.append(b""),
            Err(StoreError::Sealed { len: 3 })
        ));
        assert_eq!(store.read_range(0..3).unwrap(), b"abc");
        assert_eq!(store.len().unwrap(), 3);
    }

    #[test]
    fn concurrent_append_and_seal_have_one_linearized_winner() {
        for _ in 0..64 {
            let store = AppendStore::new();
            let barrier = Arc::new(Barrier::new(3));
            let append_store = store.clone();
            let append_barrier = Arc::clone(&barrier);
            let append = std::thread::spawn(move || {
                append_barrier.wait();
                append_store.append(b"x")
            });
            let seal_store = store.clone();
            let seal_barrier = Arc::clone(&barrier);
            let seal = std::thread::spawn(move || {
                seal_barrier.wait();
                seal_store.seal()
            });
            barrier.wait();

            let append = append.join().unwrap();
            let sealed_len = seal.join().unwrap().unwrap();
            match append {
                Ok(0) => {
                    assert_eq!(sealed_len, 1);
                    assert_eq!(store.read_range(0..1).unwrap(), b"x");
                }
                Err(StoreError::Sealed { len: 0 }) => {
                    assert_eq!(sealed_len, 0);
                    assert!(store.is_empty().unwrap());
                }
                outcome => panic!("unexpected append/seal outcome: {outcome:?}"),
            }
            assert!(store.is_sealed().unwrap());
            assert!(matches!(store.append(b"y"), Err(StoreError::Sealed { .. })));
        }
    }
}
