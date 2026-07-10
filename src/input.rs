//! stdin reader、memory tier、temporary spill store。
//!
//! `AppendStore` は受信済み byte の完全な copy を所有する。64 MiBまでは memory に保持し、次の
//! append が上限を超える直前に既存内容を permission 0600 の unlinked tempfile へ移す。parser
//! metadata や derived cache はこの module の budget に含めない。

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, RwLock};

use thiserror::Error;

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
}

/// Reader thread と background worker が共有できる append-only byte store。
#[derive(Clone, Debug)]
pub struct AppendStore {
    inner: Arc<RwLock<StoreInner>>,
    memory_limit: usize,
}

#[derive(Debug)]
enum StoreInner {
    Memory(Vec<u8>),
    Spill { file: File, len: u64 },
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
            inner: Arc::new(RwLock::new(StoreInner::Memory(Vec::new()))),
            memory_limit,
        }
    }

    /// Byte を末尾へ追加し、追加前の absolute offset を返す。
    pub fn append(&self, bytes: &[u8]) -> Result<u64, StoreError> {
        let mut inner = self.inner.write().map_err(|_| StoreError::LockPoisoned)?;
        let start = inner.len();
        let added = u64::try_from(bytes.len())
            .map_err(|_| io::Error::other("append length does not fit input offsets"))?;
        let end = start
            .checked_add(added)
            .ok_or_else(|| io::Error::other("input length overflow"))?;

        if let StoreInner::Memory(memory) = &mut *inner {
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

            let mut file = tempfile::tempfile()?;
            file.write_all(memory)?;
            file.write_all(bytes)?;
            *inner = StoreInner::Spill { file, len: end };
            return Ok(start);
        }

        if let StoreInner::Spill { file, len } = &mut *inner {
            // 直前のwrite_allが途中で失敗した場合、file末尾には未commit byteが残り得る。logical
            // lengthから上書きすれば、再試行でそのbyteを入力の一部として誤採用しない。
            file.seek(SeekFrom::Start(*len))?;
            file.write_all(bytes)?;
            *len = end;
        }
        Ok(start)
    }

    /// Absolute byte range をcopyして返す。
    pub fn read_range(&self, range: std::ops::Range<u64>) -> Result<Vec<u8>, StoreError> {
        let mut inner = self.inner.write().map_err(|_| StoreError::LockPoisoned)?;
        let len = inner.len();
        if range.start > range.end || range.end > len {
            return Err(StoreError::OutOfBounds {
                start: range.start,
                end: range.end,
                len,
            });
        }
        let requested = usize::try_from(range.end - range.start)
            .map_err(|_| io::Error::other("requested range does not fit memory"))?;

        let mut output = Vec::new();
        output
            .try_reserve_exact(requested)
            .map_err(|_| StoreError::Allocation { requested })?;
        output.resize(requested, 0);

        match &mut *inner {
            StoreInner::Memory(memory) => {
                let start = usize::try_from(range.start)
                    .map_err(|_| io::Error::other("offset does not fit memory"))?;
                let end = start
                    .checked_add(requested)
                    .ok_or_else(|| io::Error::other("requested range overflow"))?;
                output.copy_from_slice(&memory[start..end]);
                Ok(output)
            }
            StoreInner::Spill { file, .. } => {
                file.seek(SeekFrom::Start(range.start))?;
                file.read_exact(&mut output)?;
                Ok(output)
            }
        }
    }

    /// 現在受信済みの byte 数を返す。
    pub fn len(&self) -> Result<u64, StoreError> {
        let inner = self.inner.read().map_err(|_| StoreError::LockPoisoned)?;
        Ok(inner.len())
    }

    /// Store が空かどうかを返す。
    pub fn is_empty(&self) -> Result<bool, StoreError> {
        self.len().map(|length| length == 0)
    }

    /// Tempfile へspill済みかを返す。Pathは公開しない。
    pub fn is_spilled(&self) -> Result<bool, StoreError> {
        let inner = self.inner.read().map_err(|_| StoreError::LockPoisoned)?;
        Ok(matches!(*inner, StoreInner::Spill { .. }))
    }
}

impl Default for AppendStore {
    fn default() -> Self {
        Self::new()
    }
}

impl StoreInner {
    fn len(&self) -> u64 {
        match self {
            Self::Memory(bytes) => bytes.len() as u64,
            Self::Spill { len, .. } => *len,
        }
    }
}

#[cfg(test)]
mod tests {
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
}
