//! Raw input 上の logical line span と parser が確定した小さな metadata を固定長 table に保存する。
//!
//! [`DocumentLineStore`] は raw byte を所有する [`AppendStore`] と、64 byte encoding の
//! [`LogicalLineRecord`] を保存する [`SpillTable`] を束ねる。Record ID は table の append index と
//! 同じ `u64` であり、既存 ID の record を更新しない。Snapshot は取得時の record 数と raw byte 数を
//! high-water mark として固定するため、同じ store へ後から append された line を列挙・decode しない。
//!
//! Record の raw range は終端 LF を含めない。CRLF で終端された line では末尾 CR を range に含め、
//! decoder が ANSI 除去後にその CR だけを取り除く。Text は record table に複製せず、layout/search
//! worker ごとの [`LineDecoder`] が viewport または検索対象の line だけを raw store から復元する。

use std::mem::size_of;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::ansi::{AnsiStripper, EscapeError, escape_interactive_bytes_into_counted};
use crate::input::{AppendStore, StoreError};
use crate::model::LineKind;
use crate::storage::{FixedRecord, RecordCodecError, SpillTable, StorageError, StructuralStorage};

// Store identityはspanを別documentへappendできないようにするprocess内tokenであり、diskへencodeしない。
// 2^64個のstore生成はaddress spaceと実行時間を先に使い切る。
static NEXT_DOCUMENT_LINE_STORE_ID: AtomicU64 = AtomicU64::new(1);

/// `LogicalLineRecord` の disk encoding 幅。Rust struct の memory layout には依存しない。
pub const LOGICAL_LINE_RECORD_BYTES: usize = 64;

const OPTIONAL_U64_NONE: u64 = u64::MAX;
const FLAG_DISPLAY_LEN_OVERFLOW: u8 = 1 << 0;
const FLAG_TERMINATED: u8 = 1 << 1;
const KNOWN_FLAGS: u8 = FLAG_DISPLAY_LEN_OVERFLOW | FLAG_TERMINATED;

const RAW_START_OFFSET: usize = 0;
const RAW_END_OFFSET: usize = 8;
const COMMIT_ID_OFFSET: usize = 16;
const FILE_ID_OFFSET: usize = 24;
const HUNK_ID_OFFSET: usize = 32;
const OLD_LINE_OFFSET: usize = 40;
const NEW_LINE_OFFSET: usize = 48;
const DISPLAY_LEN_OFFSET: usize = 56;
const KIND_OFFSET: usize = 60;
const FLAGS_OFFSET: usize = 61;
const RESERVED_OFFSET: usize = 62;

/// [`RawLineTracker`]だけが作れるraw logical-line境界のproof token。
///
/// Terminated tokenの`raw_end`はindexed ANSI callbackがemitしたLFのraw offset、unterminated tokenの
/// `raw_end`はsealed input high-water markとなる。Storeはtoken作成後にraw byteを再読せず、5百万行でも
/// 1行1preadを発生させない。
#[derive(Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "indexed parser移行後にcrate内のboundary proofとして構築する"
)]
pub(crate) struct RawLineSpan {
    store_id: u64,
    raw_start: u64,
    raw_end: u64,
    terminated: bool,
}

/// Indexed ANSI outputをlogical-line境界へ変換するsingle-writer tracker。
///
/// Parserは[`AnsiStripper::push_indexed`]と[`AnsiStripper::finish_indexed`]のcallbackを
/// [`Self::observe`]へ直結する。OSC/DCS内のraw LFはcallbackに現れないためspanを閉じず、chunkをまたいでも
/// `raw_start`と可視byteの有無を維持する。EOFではraw storeをsealしてから[`Self::seal_eof`]を呼ぶ。
#[derive(Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "indexed parser移行後にcrate内のwriter stateとして所有する"
)]
pub(crate) struct RawLineTracker {
    store_id: u64,
    raw_start: u64,
    has_emitted_content: bool,
    last_emitted_offset: Option<u64>,
    sealed: bool,
}

#[allow(dead_code, reason = "indexed parser移行後にANSI callbackから呼び出す")]
impl RawLineTracker {
    /// 次にemitされるlogical lineが始まるraw absolute offsetからtrackerを作る。
    fn new(store_id: u64, raw_start: u64) -> Self {
        Self {
            store_id,
            raw_start,
            has_emitted_content: false,
            last_emitted_offset: None,
            sealed: false,
        }
    }

    /// Indexed ANSI callbackの1byteを取り込み、emitされたLFならterminated spanを返す。
    ///
    /// Empty lineではcontent byteがなくてもLF自体がlineを作る。LF以外のemit byteはEOF時に
    /// unterminated lineが存在することを記録する。
    pub(crate) fn observe(
        &mut self,
        raw_offset: u64,
        emitted_byte: u8,
    ) -> Result<Option<RawLineSpan>, RawLineSpanError> {
        if self.sealed {
            return Err(RawLineSpanError::TrackerSealed);
        }
        if raw_offset < self.raw_start {
            return Err(RawLineSpanError::OffsetBeforeOpenLine {
                offset: raw_offset,
                raw_start: self.raw_start,
            });
        }
        if let Some(previous) = self.last_emitted_offset
            && raw_offset <= previous
        {
            return Err(RawLineSpanError::NonMonotonicEmission {
                previous,
                offset: raw_offset,
            });
        }
        if emitted_byte != b'\n' {
            self.has_emitted_content = true;
            self.last_emitted_offset = Some(raw_offset);
            return Ok(None);
        }

        let span = RawLineSpan {
            store_id: self.store_id,
            raw_start: self.raw_start,
            raw_end: raw_offset,
            terminated: true,
        };
        self.raw_start = raw_offset
            .checked_add(1)
            .ok_or(RawLineSpanError::OffsetOverflow)?;
        self.has_emitted_content = false;
        self.last_emitted_offset = None;
        Ok(Some(span))
    }

    /// Sealed raw inputの末尾に可視byteが残る時だけunterminated spanを返す。
    ///
    /// ANSI sequenceだけがEOFまで続いた場合と、直前のemit LFでlineが閉じた場合は`None`となる。
    pub(crate) fn seal_eof(
        &mut self,
        sealed_high_water: u64,
    ) -> Result<Option<RawLineSpan>, RawLineSpanError> {
        if self.sealed {
            return Err(RawLineSpanError::TrackerSealed);
        }
        if sealed_high_water < self.raw_start {
            return Err(RawLineSpanError::OffsetBeforeOpenLine {
                offset: sealed_high_water,
                raw_start: self.raw_start,
            });
        }
        if let Some(last_emitted_offset) = self.last_emitted_offset
            && last_emitted_offset >= sealed_high_water
        {
            return Err(RawLineSpanError::EmissionAtOrPastSeal {
                last_emitted_offset,
                sealed_high_water,
            });
        }
        self.sealed = true;
        if !self.has_emitted_content {
            self.raw_start = sealed_high_water;
            self.last_emitted_offset = None;
            return Ok(None);
        }
        let span = RawLineSpan {
            store_id: self.store_id,
            raw_start: self.raw_start,
            raw_end: sealed_high_water,
            terminated: false,
        };
        self.raw_start = sealed_high_water;
        self.has_emitted_content = false;
        self.last_emitted_offset = None;
        Ok(Some(span))
    }
}

/// Indexed emissionまたはsealed EOFのoffsetがtrackerの単調増加契約を満たさない時のfailure。
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[allow(dead_code, reason = "indexed parser移行後にtrackerから返す")]
pub(crate) enum RawLineSpanError {
    #[error("indexed raw offset {offset} precedes open logical line start {raw_start}")]
    OffsetBeforeOpenLine { offset: u64, raw_start: u64 },
    #[error("indexed raw offset {offset} does not follow previous emitted offset {previous}")]
    NonMonotonicEmission { previous: u64, offset: u64 },
    #[error(
        "last emitted raw offset {last_emitted_offset} must be below sealed high-water mark {sealed_high_water}"
    )]
    EmissionAtOrPastSeal {
        last_emitted_offset: u64,
        sealed_high_water: u64,
    },
    #[error("raw line tracker was already sealed at EOF")]
    TrackerSealed,
    #[error("logical line raw offset overflow")]
    OffsetOverflow,
}

/// Parserがspan確定時に添える1 logical lineのmetadata。
///
/// Raw offsetとterminationを含めないため、decoded recordや独立したbooleanからboundary proofを
/// 再構成できない。Optional `u64`の`u64::MAX`はdisk上で`None`に予約しており、append時に拒否する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code, reason = "indexed parser移行後にwriterへmetadataを渡す")]
pub(crate) struct LogicalLineMetadata {
    /// Plain preamble では`None`。Commit配下のlineでは所有groupのstable ID。
    pub commit_id: Option<u64>,
    /// File section外では`None`。同じpathが再登場しても occurrence ごとに異なるstable ID。
    pub file_id: Option<u64>,
    /// Hunk外では`None`。Hunk headerとそのbodyでは所有hunkのstable ID。
    pub hunk_id: Option<u64>,
    /// Context/deletion lineの旧file行番号。それ以外では`None`。
    pub old_line_number: Option<u64>,
    /// Context/addition lineの新file行番号。それ以外では`None`。
    pub new_line_number: Option<u64>,
    /// ANSI 除去と byte escape 後の UTF-8 byte 数。`u32` を超える値は上限値と overflow flagで保存する。
    pub display_len: u64,
    /// Parserが確定した構造上のline分類。
    pub kind: LineKind,
}

/// Raw span と parser metadata を exactly 64 byte にencodeする logical-line record。
///
/// Field は accessor 経由でのみ読める。これにより `display_len` の saturation flag、optional sentinel、
/// reserved byte の不変条件を、RAM tier と spill tier のどちらでも同じ codec が所有する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogicalLineRecord {
    raw_start: u64,
    raw_end: u64,
    commit_id: Option<u64>,
    file_id: Option<u64>,
    hunk_id: Option<u64>,
    old_line_number: Option<u64>,
    new_line_number: Option<u64>,
    display_len: u32,
    display_len_overflow: bool,
    kind: LineKind,
    terminated: bool,
}

impl LogicalLineRecord {
    /// Boundary proofとparser metadataを検証して固定長recordを作る。
    #[allow(
        dead_code,
        reason = "record構築はindexed parserへ移行するcrate内writerだけが呼ぶ"
    )]
    fn new(span: &RawLineSpan, metadata: LogicalLineMetadata) -> Result<Self, RecordCodecError> {
        validate_optional(metadata.commit_id)?;
        validate_optional(metadata.file_id)?;
        validate_optional(metadata.hunk_id)?;
        validate_optional(metadata.old_line_number)?;
        validate_optional(metadata.new_line_number)?;

        let (display_len, display_len_overflow) = match u32::try_from(metadata.display_len) {
            Ok(display_len) => (display_len, false),
            Err(_) => (u32::MAX, true),
        };
        Ok(Self {
            raw_start: span.raw_start,
            raw_end: span.raw_end,
            commit_id: metadata.commit_id,
            file_id: metadata.file_id,
            hunk_id: metadata.hunk_id,
            old_line_number: metadata.old_line_number,
            new_line_number: metadata.new_line_number,
            display_len,
            display_len_overflow,
            kind: metadata.kind,
            terminated: span.terminated,
        })
    }

    /// LF を除く raw byte range を返す。
    #[must_use]
    pub const fn raw_start(self) -> u64 {
        self.raw_start
    }

    /// LF を除く raw byte range の exclusive end を返す。
    #[must_use]
    pub const fn raw_end(self) -> u64 {
        self.raw_end
    }

    /// 所有commit groupのstable IDを返す。Plain preambleでは`None`。
    #[must_use]
    pub const fn commit_id(self) -> Option<u64> {
        self.commit_id
    }

    /// 所有file occurrenceのstable IDを返す。File section外では`None`。
    #[must_use]
    pub const fn file_id(self) -> Option<u64> {
        self.file_id
    }

    /// 所有hunkのstable IDを返す。Hunk外では`None`。
    #[must_use]
    pub const fn hunk_id(self) -> Option<u64> {
        self.hunk_id
    }

    /// 旧file側の行番号を返す。旧側を消費しないlineでは`None`。
    #[must_use]
    pub const fn old_line_number(self) -> Option<u64> {
        self.old_line_number
    }

    /// 新file側の行番号を返す。新側を消費しないlineでは`None`。
    #[must_use]
    pub const fn new_line_number(self) -> Option<u64> {
        self.new_line_number
    }

    /// Saturation 済みの表示 UTF-8 byte 数を返す。正確性は [`Self::display_len_overflowed`] と組で読む。
    #[must_use]
    pub const fn display_len(self) -> u32 {
        self.display_len
    }

    /// 元の表示 byte 数が `u32::MAX` を超えていたかを返す。
    #[must_use]
    pub const fn display_len_overflowed(self) -> bool {
        self.display_len_overflow
    }

    /// Parserが確定した構造上のline分類を返す。
    #[must_use]
    pub const fn kind(self) -> LineKind {
        self.kind
    }

    /// Raw input 上で表示対象のLFにより閉じたlineかを返す。
    #[must_use]
    pub const fn terminated(self) -> bool {
        self.terminated
    }

    fn raw_range(self) -> Range<u64> {
        self.raw_start..self.raw_end
    }
}

impl FixedRecord for LogicalLineRecord {
    const ENCODED_LEN: usize = LOGICAL_LINE_RECORD_BYTES;

    fn encode(&self, output: &mut [u8]) -> Result<(), RecordCodecError> {
        if output.len() != Self::ENCODED_LEN {
            return Err(RecordCodecError::new(
                "logical line record output width is not 64 bytes",
            ));
        }
        write_u64(output, RAW_START_OFFSET, self.raw_start)?;
        write_u64(output, RAW_END_OFFSET, self.raw_end)?;
        write_u64(output, COMMIT_ID_OFFSET, encode_optional(self.commit_id))?;
        write_u64(output, FILE_ID_OFFSET, encode_optional(self.file_id))?;
        write_u64(output, HUNK_ID_OFFSET, encode_optional(self.hunk_id))?;
        write_u64(
            output,
            OLD_LINE_OFFSET,
            encode_optional(self.old_line_number),
        )?;
        write_u64(
            output,
            NEW_LINE_OFFSET,
            encode_optional(self.new_line_number),
        )?;
        write_u32(output, DISPLAY_LEN_OFFSET, self.display_len)?;
        output[KIND_OFFSET] = encode_kind(self.kind);
        output[FLAGS_OFFSET] = (u8::from(self.display_len_overflow) * FLAG_DISPLAY_LEN_OVERFLOW)
            | (u8::from(self.terminated) * FLAG_TERMINATED);
        output[RESERVED_OFFSET..Self::ENCODED_LEN].fill(0);
        Ok(())
    }

    fn decode(input: &[u8]) -> Result<Self, RecordCodecError> {
        if input.len() != Self::ENCODED_LEN {
            return Err(RecordCodecError::new(
                "logical line record input width is not 64 bytes",
            ));
        }
        if input[RESERVED_OFFSET..Self::ENCODED_LEN] != [0, 0] {
            return Err(RecordCodecError::new(
                "logical line record reserved bytes are non-zero",
            ));
        }
        let flags = input[FLAGS_OFFSET];
        if flags & !KNOWN_FLAGS != 0 {
            return Err(RecordCodecError::new(
                "logical line record contains unknown flags",
            ));
        }
        let raw_start = read_u64(input, RAW_START_OFFSET)?;
        let raw_end = read_u64(input, RAW_END_OFFSET)?;
        if raw_start > raw_end {
            return Err(RecordCodecError::new(
                "logical line raw start exceeds raw end",
            ));
        }
        let display_len = read_u32(input, DISPLAY_LEN_OFFSET)?;
        let display_len_overflow = flags & FLAG_DISPLAY_LEN_OVERFLOW != 0;
        if display_len_overflow && display_len != u32::MAX {
            return Err(RecordCodecError::new(
                "overflowed logical line display length is not saturated",
            ));
        }
        Ok(Self {
            raw_start,
            raw_end,
            commit_id: decode_optional(read_u64(input, COMMIT_ID_OFFSET)?),
            file_id: decode_optional(read_u64(input, FILE_ID_OFFSET)?),
            hunk_id: decode_optional(read_u64(input, HUNK_ID_OFFSET)?),
            old_line_number: decode_optional(read_u64(input, OLD_LINE_OFFSET)?),
            new_line_number: decode_optional(read_u64(input, NEW_LINE_OFFSET)?),
            display_len,
            display_len_overflow,
            kind: decode_kind(input[KIND_OFFSET])?,
            terminated: flags & FLAG_TERMINATED != 0,
        })
    }
}

#[allow(
    dead_code,
    reason = "optional metadata検証はindexed parserへ移行するwriterから呼ぶ"
)]
fn validate_optional(value: Option<u64>) -> Result<(), RecordCodecError> {
    if value == Some(OPTIONAL_U64_NONE) {
        return Err(RecordCodecError::new(
            "u64::MAX is reserved for an absent logical line field",
        ));
    }
    Ok(())
}

const fn encode_optional(value: Option<u64>) -> u64 {
    match value {
        Some(value) => value,
        None => OPTIONAL_U64_NONE,
    }
}

const fn decode_optional(value: u64) -> Option<u64> {
    if value == OPTIONAL_U64_NONE {
        None
    } else {
        Some(value)
    }
}

fn write_u64(output: &mut [u8], offset: usize, value: u64) -> Result<(), RecordCodecError> {
    let end = offset
        .checked_add(size_of::<u64>())
        .ok_or_else(|| RecordCodecError::new("logical line record field offset overflow"))?;
    let destination = output
        .get_mut(offset..end)
        .ok_or_else(|| RecordCodecError::new("logical line record u64 field is out of bounds"))?;
    destination.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u32(output: &mut [u8], offset: usize, value: u32) -> Result<(), RecordCodecError> {
    let end = offset
        .checked_add(size_of::<u32>())
        .ok_or_else(|| RecordCodecError::new("logical line record field offset overflow"))?;
    let destination = output
        .get_mut(offset..end)
        .ok_or_else(|| RecordCodecError::new("logical line record u32 field is out of bounds"))?;
    destination.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn read_u64(input: &[u8], offset: usize) -> Result<u64, RecordCodecError> {
    let end = offset
        .checked_add(size_of::<u64>())
        .ok_or_else(|| RecordCodecError::new("logical line record field offset overflow"))?;
    let bytes = input
        .get(offset..end)
        .ok_or_else(|| RecordCodecError::new("logical line record u64 field is out of bounds"))?;
    let bytes = <[u8; 8]>::try_from(bytes)
        .map_err(|_| RecordCodecError::new("logical line record u64 field has invalid width"))?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_u32(input: &[u8], offset: usize) -> Result<u32, RecordCodecError> {
    let end = offset
        .checked_add(size_of::<u32>())
        .ok_or_else(|| RecordCodecError::new("logical line record field offset overflow"))?;
    let bytes = input
        .get(offset..end)
        .ok_or_else(|| RecordCodecError::new("logical line record u32 field is out of bounds"))?;
    let bytes = <[u8; 4]>::try_from(bytes)
        .map_err(|_| RecordCodecError::new("logical line record u32 field has invalid width"))?;
    Ok(u32::from_le_bytes(bytes))
}

const fn encode_kind(kind: LineKind) -> u8 {
    match kind {
        LineKind::Plain => 0,
        LineKind::CommitHeader => 1,
        LineKind::CommitMetadata => 2,
        LineKind::CommitMessage => 3,
        LineKind::DiffHeader => 4,
        LineKind::FileMetadata => 5,
        LineKind::HunkHeader => 6,
        LineKind::Context => 7,
        LineKind::Deletion => 8,
        LineKind::Addition => 9,
        LineKind::NoNewlineMarker => 10,
        LineKind::Binary => 11,
        LineKind::Submodule => 12,
        LineKind::Fallback => 13,
    }
}

const fn decode_kind(encoded: u8) -> Result<LineKind, RecordCodecError> {
    match encoded {
        0 => Ok(LineKind::Plain),
        1 => Ok(LineKind::CommitHeader),
        2 => Ok(LineKind::CommitMetadata),
        3 => Ok(LineKind::CommitMessage),
        4 => Ok(LineKind::DiffHeader),
        5 => Ok(LineKind::FileMetadata),
        6 => Ok(LineKind::HunkHeader),
        7 => Ok(LineKind::Context),
        8 => Ok(LineKind::Deletion),
        9 => Ok(LineKind::Addition),
        10 => Ok(LineKind::NoNewlineMarker),
        11 => Ok(LineKind::Binary),
        12 => Ok(LineKind::Submodule),
        13 => Ok(LineKind::Fallback),
        _ => Err(RecordCodecError::new(
            "logical line record contains an unknown line kind",
        )),
    }
}

/// Logical-line table、raw range、lazy decode の failure。
#[derive(Debug, Error)]
pub enum LineStoreError {
    #[error(transparent)]
    RecordCodec(#[from] RecordCodecError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Input(#[from] StoreError),
    #[error(transparent)]
    Escape(#[from] EscapeError),
    #[error("document line writer lock was poisoned")]
    WriterLockPoisoned,
    #[error("document line tracker was already started")]
    TrackerAlreadyStarted,
    #[error("raw line span belongs to store {actual}, not store {expected}")]
    SpanStoreMismatch { expected: u64, actual: u64 },
    #[error("raw line span starts at {actual}, but next append must start at {expected}")]
    NonContiguousSpan { expected: u64, actual: u64 },
    #[error("an unterminated terminal record was already appended")]
    TerminalRecordAlreadyAppended,
    #[error(
        "logical line raw range {start}..{end} exceeds available raw input ending at {raw_len}"
    )]
    RawRangeOutOfBounds { start: u64, end: u64, raw_len: u64 },
    #[error(
        "terminated logical line raw end {raw_end} must be below input high-water mark {raw_len}"
    )]
    TerminatedEndNotBelowHighWater { raw_end: u64, raw_len: u64 },
    #[error(
        "unterminated logical line raw end {raw_end} must equal sealed input high-water mark {raw_len}"
    )]
    UnterminatedEndNotAtHighWater { raw_end: u64, raw_len: u64 },
    #[error(
        "unterminated logical line ending at {raw_end} requires a sealed raw input (current length {raw_len})"
    )]
    UnterminatedInputNotSealed { raw_end: u64, raw_len: u64 },
    #[error(
        "logical line record {line_id} disappeared below snapshot high-water mark {high_water}"
    )]
    MissingRecord { line_id: u64, high_water: u64 },
    #[error("could not allocate {requested} bytes for ANSI-stripped logical line data")]
    DecodeAllocation { requested: usize },
    #[error(
        "logical line {line_id} stores display length {stored} (overflow={overflow}) but decodes to {actual} bytes"
    )]
    DisplayLengthMismatch {
        line_id: u64,
        stored: u32,
        overflow: bool,
        actual: usize,
    },
}

/// Raw input と append-only logical-line table の共有 handle。
///
/// Callerは[`Self::new`]へ渡した[`AppendStore`]のcloneへraw bytesを先にappendし、indexed ANSI
/// callbackから得たspanを[`Self::append`]へ渡す。Record append後にraw rangeやmetadataを上書きする
/// APIは持たない。
#[derive(Clone)]
pub struct DocumentLineStore {
    store_id: u64,
    raw: AppendStore,
    records: SpillTable<LogicalLineRecord>,
    writer: Arc<Mutex<WriterState>>,
}

#[derive(Debug, Default)]
struct WriterState {
    /// 次に受理するspanのraw start。Record append成功後だけ進める。
    next_raw_start: u64,
    /// 同じstore identityから2個のtrackerを発行しないためのlatch。
    tracker_started: bool,
    /// Unterminated EOF recordをappendした後、追加recordを拒否するlatch。
    terminal_record: bool,
}

impl DocumentLineStore {
    /// 既存 raw store と structural budget を共有する空の line table を作る。
    pub fn new(raw: AppendStore, storage: &StructuralStorage) -> Result<Self, LineStoreError> {
        Ok(Self {
            store_id: NEXT_DOCUMENT_LINE_STORE_ID.fetch_add(1, Ordering::Relaxed),
            raw,
            records: storage.table()?,
            writer: Arc::new(Mutex::new(WriterState::default())),
        })
    }

    /// このstoreだけへspanを発行できるsingle trackerを開始する。
    ///
    /// Cloneを含め2回目の呼び出しは拒否する。これにより別trackerが同じraw rangeを再発行し、duplicate
    /// recordをappendする経路を閉じる。
    #[allow(
        dead_code,
        reason = "indexed parser移行後にreader/parser境界で1回だけ呼ぶ"
    )]
    pub(crate) fn start_tracker(&self) -> Result<RawLineTracker, LineStoreError> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| LineStoreError::WriterLockPoisoned)?;
        if writer.tracker_started {
            return Err(LineStoreError::TrackerAlreadyStarted);
        }
        writer.tracker_started = true;
        Ok(RawLineTracker::new(self.store_id, writer.next_raw_start))
    }

    /// Spanとmetadataからrecordを末尾へ追加し、process lifetime中に変わらない0始まりの`u64` IDを返す。
    ///
    /// Terminated recordの`raw_end`はindexed callbackがemitしたraw LF offsetであり、現在のraw
    /// high-water markより小さくなければならない。Unterminated recordはsealed EOF tokenから作られ、
    /// `raw_end`がhigh-water markと一致する。検証はcached lengthだけを読み、raw storeへpreadしない。
    #[allow(dead_code, reason = "indexed parser移行後に唯一のappend callerとなる")]
    pub(crate) fn append(
        &self,
        span: RawLineSpan,
        metadata: LogicalLineMetadata,
    ) -> Result<u64, LineStoreError> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| LineStoreError::WriterLockPoisoned)?;
        if writer.terminal_record {
            return Err(LineStoreError::TerminalRecordAlreadyAppended);
        }
        if span.store_id != self.store_id {
            return Err(LineStoreError::SpanStoreMismatch {
                expected: self.store_id,
                actual: span.store_id,
            });
        }
        if span.raw_start != writer.next_raw_start {
            return Err(LineStoreError::NonContiguousSpan {
                expected: writer.next_raw_start,
                actual: span.raw_start,
            });
        }
        let record = LogicalLineRecord::new(&span, metadata)?;
        let raw_status = self.raw.status()?;
        validate_record_boundary(record, raw_status.len, raw_status.sealed)?;
        let next_raw_start = if record.terminated() {
            record
                .raw_end()
                .checked_add(1)
                .ok_or(LineStoreError::RawRangeOutOfBounds {
                    start: record.raw_start(),
                    end: record.raw_end(),
                    raw_len: raw_status.len,
                })?
        } else {
            record.raw_end()
        };
        let id = self.records.append(&record)?;
        writer.next_raw_start = next_raw_start;
        writer.terminal_record = !record.terminated();
        Ok(id)
    }

    /// 現時点の line 数を返す。後続 append で増加する。
    pub fn len(&self) -> Result<u64, LineStoreError> {
        self.records.len().map_err(LineStoreError::from)
    }

    /// 現時点で record がないかを返す。
    pub fn is_empty(&self) -> Result<bool, LineStoreError> {
        self.records.is_empty().map_err(LineStoreError::from)
    }

    /// Logical-line table の1件以上が temporary spill file に保存されたかを返す。
    pub fn is_spilled(&self) -> Result<bool, LineStoreError> {
        self.records.is_spilled().map_err(LineStoreError::from)
    }

    /// Record 数を固定した cheap snapshot を取得する。
    ///
    /// Raw store と table は `Arc` cloneで共有するが、`high_water` 以上の table entryを accessorが
    /// 拒否する。`raw_high_water` も取得時に固定し、snapshot後の raw appendをdecode対象にしない。
    pub fn snapshot(&self) -> Result<LineSnapshot, LineStoreError> {
        let high_water = self.records.len()?;
        let raw_status = self.raw.status()?;
        Ok(LineSnapshot {
            raw: self.raw.clone(),
            records: self.records.clone(),
            high_water,
            raw_high_water: raw_status.len,
            raw_sealed: raw_status.sealed,
        })
    }
}

/// 取得時の logical-line high-water mark を固定する read-only view。
#[derive(Clone)]
pub struct LineSnapshot {
    raw: AppendStore,
    records: SpillTable<LogicalLineRecord>,
    high_water: u64,
    raw_high_water: u64,
    /// Unterminated recordのboundary検証に使う、snapshot取得時のirreversible seal状態。
    raw_sealed: bool,
}

impl LineSnapshot {
    /// Snapshot に含まれる logical-line 数を返す。
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.high_water
    }

    /// Snapshotにlogical lineが含まれないかを返す。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.high_water == 0
    }

    /// Stable ID の record を返す。Snapshot取得後に追加された ID は `None` になる。
    pub fn record(&self, line_id: u64) -> Result<Option<LogicalLineRecord>, LineStoreError> {
        let mut buffer = Vec::new();
        self.record_with_buffer(line_id, &mut buffer)
    }

    /// Stable IDのrecordを返し、spill read用bufferのcapacityをcaller間で再利用する。
    ///
    /// RAM tierのrecordでは`buffer`を変更しない。Disk tierではexactly 64 byteを読み、次のmetadata readで
    /// 同じallocationを再利用する。Text decodeが不要なnavigation/index workerは[`LineDecoder`]を
    /// 作らずにこのAPIを使える。
    pub fn record_with_buffer(
        &self,
        line_id: u64,
        buffer: &mut Vec<u8>,
    ) -> Result<Option<LogicalLineRecord>, LineStoreError> {
        if line_id >= self.high_water {
            return Ok(None);
        }
        self.records
            .get_with_buffer(line_id, buffer)?
            .ok_or(LineStoreError::MissingRecord {
                line_id,
                high_water: self.high_water,
            })
            .map(Some)
    }
}

/// Decoder bufferを借用する1 logical line。次のdecode呼び出しまで有効で、heap allocationを所有しない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodedLine<'a> {
    /// `DocumentLineStore::append` が返したstable ID。
    pub id: u64,
    /// ANSIを除去し、invalid/control byteをescapeしたdecoder所有bufferへのview。
    pub text: &'a str,
    /// Textと同じlineに属するimmutable structural metadata。
    pub record: LogicalLineRecord,
}

impl<'a> DecodedLine<'a> {
    /// Diff source line では先頭 marker を除いた本文、それ以外では行全体を返す。
    #[must_use]
    pub fn content(self) -> &'a str {
        if matches!(
            self.record.kind(),
            LineKind::Context | LineKind::Deletion | LineKind::Addition
        ) {
            self.text.get(1..).unwrap_or_default()
        } else {
            self.text
        }
    }

    /// Background jobやcacheがdecoderの次回利用後も保持できる owned valueを作る。
    pub fn try_to_owned(self) -> Result<OwnedDecodedLine, LineStoreError> {
        let mut text = String::new();
        text.try_reserve_exact(self.text.len())
            .map_err(|_| LineStoreError::DecodeAllocation {
                requested: self.text.len(),
            })?;
        text.push_str(self.text);
        Ok(OwnedDecodedLine {
            id: self.id,
            text,
            record: self.record,
        })
    }
}

/// Decoder bufferから独立して保持できる1 logical line。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedDecodedLine {
    /// `DocumentLineStore::append` が返したstable ID。
    pub id: u64,
    /// ANSIを除去し、invalid/control byteをescapeしたowned UTF-8 text。
    pub text: String,
    /// Textと同じlineに属するimmutable structural metadata。
    pub record: LogicalLineRecord,
}

impl OwnedDecodedLine {
    /// Allocationなしで borrowed view を作る。
    #[must_use]
    pub fn as_borrowed(&self) -> DecodedLine<'_> {
        DecodedLine {
            id: self.id,
            text: &self.text,
            record: self.record,
        }
    }

    /// Diff source line では先頭 marker を除いた本文、それ以外では行全体を返す。
    #[must_use]
    pub fn content(&self) -> &str {
        self.as_borrowed().content()
    }
}

/// Snapshot の raw spanを表示textへ遅延変換する、workerごとの再利用可能buffer群。
///
/// `AnsiStripper` はlineごとにGround状態から開始する。Parserがrecordしたlogical-line境界は表示上の
/// LFで閉じているため、前のdecodeが途中のCSI/OSC stateを次のlineへ持ち越してはならない。
#[derive(Debug, Default)]
pub struct LineDecoder {
    record_buffer: Vec<u8>,
    raw_buffer: Vec<u8>,
    stripped_buffer: Vec<u8>,
    text_buffer: String,
}

impl LineDecoder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            record_buffer: Vec::new(),
            raw_buffer: Vec::new(),
            stripped_buffer: Vec::new(),
            text_buffer: String::new(),
        }
    }

    /// Stable IDをsnapshotから読み、ANSI/control byteを実行不能なborrowed textへ変換する。
    ///
    /// Snapshot外のIDは`Ok(None)`。Record/raw read、buffer確保、byte escapeのfailureはerrorとして返し、
    /// panicへ変換しない。返したtextは`self`を借用するため、次のdecodeとは同時に保持できない。
    pub fn decode<'a>(
        &'a mut self,
        snapshot: &LineSnapshot,
        line_id: u64,
    ) -> Result<Option<DecodedLine<'a>>, LineStoreError> {
        if line_id >= snapshot.high_water {
            return Ok(None);
        }
        let record = snapshot
            .records
            .get_with_buffer(line_id, &mut self.record_buffer)?
            .ok_or(LineStoreError::MissingRecord {
                line_id,
                high_water: snapshot.high_water,
            })?;
        validate_record_boundary(record, snapshot.raw_high_water, snapshot.raw_sealed)?;
        snapshot
            .raw
            .read_range_into(record.raw_range(), &mut self.raw_buffer)?;

        self.stripped_buffer.clear();
        self.stripped_buffer
            .try_reserve_exact(self.raw_buffer.len())
            .map_err(|_| LineStoreError::DecodeAllocation {
                requested: self.raw_buffer.len(),
            })?;
        let mut stripper = AnsiStripper::default();
        stripper.push(&self.raw_buffer, &mut self.stripped_buffer);
        stripper.finish_into(&mut self.stripped_buffer);

        // Parserのlogical-line分割はANSI除去後のLFを境界にする。CRとraw LFの間にSGR resetが
        // 置かれていてもsanitized lineはCRで終わるため、raw末尾ではなく除去後の末尾を検査する。
        if record.terminated() && self.stripped_buffer.last() == Some(&b'\r') {
            let _ = self.stripped_buffer.pop();
        }
        let actual_display_len =
            escape_interactive_bytes_into_counted(&self.stripped_buffer, &mut self.text_buffer)?;
        let actual_u32 = u32::try_from(actual_display_len).ok();
        let stored_matches = if record.display_len_overflowed() {
            actual_u32.is_none()
        } else {
            actual_u32 == Some(record.display_len())
        };
        if !stored_matches {
            return Err(LineStoreError::DisplayLengthMismatch {
                line_id,
                stored: record.display_len(),
                overflow: record.display_len_overflowed(),
                actual: actual_display_len,
            });
        }

        Ok(Some(DecodedLine {
            id: line_id,
            text: &self.text_buffer,
            record,
        }))
    }
}

fn validate_record_boundary(
    record: LogicalLineRecord,
    raw_len: u64,
    raw_sealed: bool,
) -> Result<(), LineStoreError> {
    if record.raw_start() > record.raw_end() || record.raw_end() > raw_len {
        return Err(LineStoreError::RawRangeOutOfBounds {
            start: record.raw_start(),
            end: record.raw_end(),
            raw_len,
        });
    }
    if record.terminated() && record.raw_end() >= raw_len {
        return Err(LineStoreError::TerminatedEndNotBelowHighWater {
            raw_end: record.raw_end(),
            raw_len,
        });
    }
    if !record.terminated() && !raw_sealed {
        return Err(LineStoreError::UnterminatedInputNotSealed {
            raw_end: record.raw_end(),
            raw_len,
        });
    }
    if !record.terminated() && record.raw_end() != raw_len {
        return Err(LineStoreError::UnterminatedEndNotAtHighWater {
            raw_end: record.raw_end(),
            raw_len,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::ansi::{escape_interactive_bytes, strip_ansi};
    use crate::storage::StorageConfig;

    fn metadata(display_len: u64, kind: LineKind) -> LogicalLineMetadata {
        LogicalLineMetadata {
            commit_id: None,
            file_id: None,
            hunk_id: None,
            old_line_number: None,
            new_line_number: None,
            display_len,
            kind,
        }
    }

    fn codec_span(raw_start: u64, raw_end: u64, terminated: bool) -> RawLineSpan {
        RawLineSpan {
            store_id: 0,
            raw_start,
            raw_end,
            terminated,
        }
    }

    fn collect_spans(store: &DocumentLineStore, chunks: &[&[u8]]) -> Vec<RawLineSpan> {
        let mut stripper = AnsiStripper::default();
        let mut tracker = store.start_tracker().unwrap();
        let mut spans = Vec::new();
        let mut raw_base = 0_u64;
        for chunk in chunks {
            stripper
                .push_indexed(raw_base, chunk, |offset, byte| {
                    if let Some(span) = tracker.observe(offset, byte).unwrap() {
                        spans.push(span);
                    }
                })
                .unwrap();
            raw_base += chunk.len() as u64;
        }
        stripper.finish_indexed(|offset, byte| {
            if let Some(span) = tracker.observe(offset, byte).unwrap() {
                spans.push(span);
            }
        });
        if let Some(span) = tracker.seal_eof(raw_base).unwrap() {
            spans.push(span);
        }
        spans
    }

    fn default_store(raw: AppendStore) -> DocumentLineStore {
        let storage = StructuralStorage::new().unwrap();
        DocumentLineStore::new(raw, &storage).unwrap()
    }

    fn tracker_spans(chunks: &[&[u8]]) -> Vec<RawLineSpan> {
        let store = default_store(AppendStore::new());
        collect_spans(&store, chunks)
    }

    fn eager_plain_lines(input: &[u8]) -> Vec<String> {
        let sanitized = strip_ansi(input);
        let mut lines = Vec::new();
        let mut start = 0_usize;
        for (index, &byte) in sanitized.iter().enumerate() {
            if byte != b'\n' {
                continue;
            }
            let end = if index > start && sanitized[index - 1] == b'\r' {
                index - 1
            } else {
                index
            };
            lines.push(escape_interactive_bytes(&sanitized[start..end]));
            start = index + 1;
        }
        if start < sanitized.len() {
            lines.push(escape_interactive_bytes(&sanitized[start..]));
        }
        lines
    }

    #[test]
    fn codec_is_exactly_64_bytes_and_uses_explicit_sentinels() {
        assert_eq!(LogicalLineRecord::ENCODED_LEN, 64);
        let span = codec_span(0x0102_0304_0506_0708, 0x1112_1314_1516_1718, true);
        let record = LogicalLineRecord::new(
            &span,
            LogicalLineMetadata {
                commit_id: Some(1),
                file_id: Some(2),
                hunk_id: Some(3),
                old_line_number: Some(4),
                new_line_number: Some(5),
                display_len: u64::from(u32::MAX) + 1,
                kind: LineKind::Addition,
            },
        )
        .unwrap();
        let mut encoded = [0xaa; LOGICAL_LINE_RECORD_BYTES];
        record.encode(&mut encoded).unwrap();

        assert_eq!(&encoded[0..8], &record.raw_start().to_le_bytes());
        assert_eq!(&encoded[8..16], &record.raw_end().to_le_bytes());
        assert_eq!(&encoded[16..24], &1_u64.to_le_bytes());
        assert_eq!(&encoded[24..32], &2_u64.to_le_bytes());
        assert_eq!(&encoded[32..40], &3_u64.to_le_bytes());
        assert_eq!(&encoded[40..48], &4_u64.to_le_bytes());
        assert_eq!(&encoded[48..56], &5_u64.to_le_bytes());
        assert_eq!(&encoded[56..60], &u32::MAX.to_le_bytes());
        assert_eq!(encoded[60], 9);
        assert_eq!(encoded[61], FLAG_DISPLAY_LEN_OVERFLOW | FLAG_TERMINATED);
        assert_eq!(&encoded[62..64], &[0, 0]);
        assert_eq!(LogicalLineRecord::decode(&encoded).unwrap(), record);

        let absent =
            LogicalLineRecord::new(&codec_span(0, 0, true), metadata(0, LineKind::Plain)).unwrap();
        absent.encode(&mut encoded).unwrap();
        for offset in [16, 24, 32, 40, 48] {
            assert_eq!(&encoded[offset..offset + 8], &u64::MAX.to_le_bytes());
        }
        assert_eq!(LogicalLineRecord::decode(&encoded).unwrap(), absent);
        assert!(
            LogicalLineRecord::new(
                &codec_span(0, 0, true),
                LogicalLineMetadata {
                    commit_id: Some(u64::MAX),
                    ..metadata(0, LineKind::Plain)
                },
            )
            .is_err()
        );
    }

    #[test]
    fn codec_rejects_unknown_kind_flags_and_reserved_bytes() {
        let record =
            LogicalLineRecord::new(&codec_span(0, 0, true), metadata(0, LineKind::Plain)).unwrap();
        let mut encoded = [0; LOGICAL_LINE_RECORD_BYTES];
        record.encode(&mut encoded).unwrap();
        encoded[KIND_OFFSET] = 0xff;
        assert!(LogicalLineRecord::decode(&encoded).is_err());

        record.encode(&mut encoded).unwrap();
        encoded[FLAGS_OFFSET] = 0x80;
        assert!(LogicalLineRecord::decode(&encoded).is_err());

        record.encode(&mut encoded).unwrap();
        encoded[FLAGS_OFFSET] = FLAG_DISPLAY_LEN_OVERFLOW;
        assert!(LogicalLineRecord::decode(&encoded).is_err());

        record.encode(&mut encoded).unwrap();
        encoded[RESERVED_OFFSET] = 1;
        assert!(LogicalLineRecord::decode(&encoded).is_err());
    }

    #[test]
    fn codec_rejects_63_and_65_byte_buffers_and_reversed_raw_range() {
        let record =
            LogicalLineRecord::new(&codec_span(0, 1, true), metadata(1, LineKind::Plain)).unwrap();
        let mut short = [0_u8; 63];
        let mut long = [0_u8; 65];
        assert!(record.encode(&mut short).is_err());
        assert!(record.encode(&mut long).is_err());
        assert!(LogicalLineRecord::decode(&short).is_err());
        assert!(LogicalLineRecord::decode(&long).is_err());

        let mut encoded = [0_u8; LOGICAL_LINE_RECORD_BYTES];
        record.encode(&mut encoded).unwrap();
        encoded[RAW_START_OFFSET..RAW_START_OFFSET + 8].copy_from_slice(&2_u64.to_le_bytes());
        encoded[RAW_END_OFFSET..RAW_END_OFFSET + 8].copy_from_slice(&1_u64.to_le_bytes());
        assert!(LogicalLineRecord::decode(&encoded).is_err());
    }

    #[test]
    fn codec_rejects_the_sentinel_in_every_optional_field() {
        for field in 0..5 {
            let mut value = metadata(1, LineKind::Plain);
            match field {
                0 => value.commit_id = Some(u64::MAX),
                1 => value.file_id = Some(u64::MAX),
                2 => value.hunk_id = Some(u64::MAX),
                3 => value.old_line_number = Some(u64::MAX),
                4 => value.new_line_number = Some(u64::MAX),
                _ => unreachable!(),
            }
            assert!(LogicalLineRecord::new(&codec_span(0, 1, true), value).is_err());
        }
    }

    #[test]
    fn exact_u32_max_display_length_does_not_set_overflow_flag() {
        let exact = LogicalLineRecord::new(
            &codec_span(0, 1, true),
            LogicalLineMetadata {
                display_len: u64::from(u32::MAX),
                ..metadata(0, LineKind::Plain)
            },
        )
        .unwrap();
        let overflow = LogicalLineRecord::new(
            &codec_span(0, 1, true),
            LogicalLineMetadata {
                display_len: u64::from(u32::MAX) + 1,
                ..metadata(0, LineKind::Plain)
            },
        )
        .unwrap();
        let mut encoded = [0_u8; LOGICAL_LINE_RECORD_BYTES];
        exact.encode(&mut encoded).unwrap();
        assert_eq!(exact.display_len(), u32::MAX);
        assert!(!exact.display_len_overflowed());
        assert_eq!(encoded[FLAGS_OFFSET], FLAG_TERMINATED);
        assert_eq!(LogicalLineRecord::decode(&encoded).unwrap(), exact);

        overflow.encode(&mut encoded).unwrap();
        assert_eq!(overflow.display_len(), u32::MAX);
        assert!(overflow.display_len_overflowed());
        assert_eq!(
            encoded[FLAGS_OFFSET],
            FLAG_DISPLAY_LEN_OVERFLOW | FLAG_TERMINATED
        );
        assert_eq!(LogicalLineRecord::decode(&encoded).unwrap(), overflow);
    }

    #[test]
    fn raw_line_tracker_uses_only_indexed_emissions_across_chunks() {
        let spans = tracker_spans(&[
            b"\x1b]title\n".as_slice(),
            b"hidden\x07\r".as_slice(),
            b"\x1b[0m\n".as_slice(),
        ]);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].raw_start, 0);
        assert_eq!(spans[0].raw_end, 20);
        assert!(spans[0].terminated);

        let empty = tracker_spans(&[b"\n"]);
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].raw_start, 0);
        assert_eq!(empty[0].raw_end, 0);
        assert!(empty[0].terminated);
        assert!(tracker_spans(&[b"\x1b]unterminated\n"]).is_empty());
        let lone_cr = tracker_spans(&[b"\r"]);
        assert_eq!(lone_cr.len(), 1);
        assert_eq!(lone_cr[0].raw_start, 0);
        assert_eq!(lone_cr[0].raw_end, 1);
        assert!(!lone_cr[0].terminated);

        let store = default_store(AppendStore::new());
        let mut non_monotonic = store.start_tracker().unwrap();
        non_monotonic.observe(4, b'x').unwrap();
        assert!(matches!(
            non_monotonic.observe(4, b'y'),
            Err(RawLineSpanError::NonMonotonicEmission { .. })
        ));
    }

    #[test]
    fn raw_line_tracker_rejects_invalid_or_repeated_seals() {
        let store = default_store(AppendStore::new());
        let mut tracker = store.start_tracker().unwrap();
        tracker.observe(10, b'x').unwrap();
        assert!(matches!(
            tracker.seal_eof(10),
            Err(RawLineSpanError::EmissionAtOrPastSeal { .. })
        ));
        assert!(matches!(
            tracker.seal_eof(5),
            Err(RawLineSpanError::EmissionAtOrPastSeal { .. })
        ));
        assert!(tracker.seal_eof(11).unwrap().is_some());
        assert!(matches!(
            tracker.observe(11, b'!'),
            Err(RawLineSpanError::TrackerSealed)
        ));
        assert!(matches!(
            tracker.seal_eof(11),
            Err(RawLineSpanError::TrackerSealed)
        ));
    }

    #[test]
    fn tracker_and_spans_are_bound_to_one_contiguous_store_writer() {
        let raw = AppendStore::new();
        raw.append(b"a\nb\n").unwrap();
        let store = default_store(raw);
        let clone = store.clone();
        let mut tracker = store.start_tracker().unwrap();
        assert!(matches!(
            clone.start_tracker(),
            Err(LineStoreError::TrackerAlreadyStarted)
        ));
        let first = tracker.observe(1, b'\n').unwrap().unwrap();
        let second = tracker.observe(3, b'\n').unwrap().unwrap();
        assert!(matches!(
            store.append(second, metadata(1, LineKind::Plain)),
            Err(LineStoreError::NonContiguousSpan {
                expected: 0,
                actual: 2
            })
        ));
        assert_eq!(
            store.append(first, metadata(1, LineKind::Plain)).unwrap(),
            0
        );

        let foreign_raw = AppendStore::new();
        foreign_raw.append(b"x\n").unwrap();
        let foreign = default_store(foreign_raw);
        let foreign_span = collect_spans(&foreign, &[b"x\n"])
            .into_iter()
            .next()
            .unwrap();
        assert!(matches!(
            store.append(foreign_span, metadata(1, LineKind::Plain)),
            Err(LineStoreError::SpanStoreMismatch { .. })
        ));
    }

    #[test]
    fn sealed_unterminated_record_remains_decodable_across_snapshots() {
        let raw = AppendStore::new();
        raw.append(b"tail").unwrap();
        let store = default_store(raw.clone());
        let span = collect_spans(&store, &[b"tail"])
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(raw.seal().unwrap(), 4);
        assert_eq!(store.append(span, metadata(4, LineKind::Plain)).unwrap(), 0);

        let first = store.snapshot().unwrap();
        let mut decoder = LineDecoder::new();
        assert_eq!(decoder.decode(&first, 0).unwrap().unwrap().text, "tail");
        assert!(matches!(
            raw.append(b"later"),
            Err(StoreError::Sealed { len: 4 })
        ));
        let second = store.snapshot().unwrap();
        assert_eq!(decoder.decode(&second, 0).unwrap().unwrap().text, "tail");
        assert_eq!(first.raw_high_water, second.raw_high_water);
        assert!(first.raw_sealed && second.raw_sealed);

        let forged_after_terminal = RawLineSpan {
            store_id: store.store_id,
            raw_start: 4,
            raw_end: 4,
            terminated: false,
        };
        assert!(matches!(
            store.append(forged_after_terminal, metadata(0, LineKind::Plain)),
            Err(LineStoreError::TerminalRecordAlreadyAppended)
        ));
    }

    #[test]
    fn terminated_snapshot_taken_before_seal_stays_valid_after_seal() {
        let raw = AppendStore::new();
        raw.append(b"done\n").unwrap();
        let store = default_store(raw.clone());
        let span = collect_spans(&store, &[b"done\n"])
            .into_iter()
            .next()
            .unwrap();
        store.append(span, metadata(4, LineKind::Plain)).unwrap();
        let before = store.snapshot().unwrap();
        assert!(!before.raw_sealed);
        raw.seal().unwrap();
        let after = store.snapshot().unwrap();
        assert!(after.raw_sealed);

        let mut decoder = LineDecoder::new();
        assert_eq!(decoder.decode(&before, 0).unwrap().unwrap().text, "done");
        assert_eq!(decoder.decode(&after, 0).unwrap().unwrap().text, "done");
    }

    #[test]
    fn snapshot_does_not_observe_later_line_appends() {
        let raw = AppendStore::new();
        raw.append(b"first\nsecond\n").unwrap();
        let store = default_store(raw);
        let mut spans = collect_spans(&store, &[b"first\nsecond\n"]).into_iter();
        assert_eq!(
            store
                .append(spans.next().unwrap(), metadata(5, LineKind::Plain))
                .unwrap(),
            0
        );
        let earlier = store.snapshot().unwrap();
        assert_eq!(
            store
                .append(spans.next().unwrap(), metadata(6, LineKind::Plain))
                .unwrap(),
            1
        );
        let later = store.snapshot().unwrap();

        assert_eq!(earlier.len(), 1);
        assert_eq!(earlier.record(1).unwrap(), None);
        assert_eq!(later.len(), 2);
        assert_eq!(later.record(1).unwrap().unwrap().raw_start(), 6);
        let mut decoder = LineDecoder::new();
        assert!(decoder.decode(&earlier, 1).unwrap().is_none());
        assert_eq!(decoder.decode(&later, 1).unwrap().unwrap().text, "second");
    }

    #[test]
    fn invalid_termination_boundaries_do_not_consume_a_line_id() {
        let store = default_store(AppendStore::new());
        let mut false_terminator = store.start_tracker().unwrap();
        assert_eq!(false_terminator.observe(1, b'X').unwrap(), None);

        let raw = AppendStore::new();
        raw.append(b"x\n").unwrap();
        let store = default_store(raw);
        let mut tracker = store.start_tracker().unwrap();
        tracker.observe(0, b'x').unwrap();
        let beyond_raw = tracker.observe(3, b'\n').unwrap().unwrap();
        assert!(matches!(
            store.append(beyond_raw, metadata(1, LineKind::Plain)),
            Err(LineStoreError::RawRangeOutOfBounds { .. })
        ));

        let raw = AppendStore::new();
        raw.append(b"x\n").unwrap();
        let store = default_store(raw);
        let mut tracker = store.start_tracker().unwrap();
        tracker.observe(0, b'x').unwrap();
        let at_high_water = tracker.observe(2, b'\n').unwrap().unwrap();
        assert!(matches!(
            store.append(at_high_water, metadata(1, LineKind::Plain)),
            Err(LineStoreError::TerminatedEndNotBelowHighWater { .. })
        ));

        let raw = AppendStore::new();
        raw.append(b"x").unwrap();
        let store = default_store(raw);
        let mut tracker = store.start_tracker().unwrap();
        tracker.observe(0, b'x').unwrap();
        let unsealed_eof = tracker.seal_eof(1).unwrap().unwrap();
        assert!(matches!(
            store.append(unsealed_eof, metadata(1, LineKind::Plain)),
            Err(LineStoreError::UnterminatedInputNotSealed { .. })
        ));

        let raw = AppendStore::new();
        raw.append(b"x\n").unwrap();
        raw.seal().unwrap();
        let store = default_store(raw);
        let mut tracker = store.start_tracker().unwrap();
        tracker.observe(0, b'x').unwrap();
        let early_eof = tracker.seal_eof(1).unwrap().unwrap();
        assert!(matches!(
            store.append(early_eof, metadata(1, LineKind::Plain)),
            Err(LineStoreError::UnterminatedEndNotAtHighWater { .. })
        ));

        let raw = AppendStore::new();
        raw.append(b"x\n").unwrap();
        let store = default_store(raw);
        let span = collect_spans(&store, &[b"x\n"]).into_iter().next().unwrap();
        assert_eq!(store.append(span, metadata(1, LineKind::Plain)).unwrap(), 0);
    }

    #[test]
    fn metadata_reads_reuse_one_buffer_across_forced_record_spill() {
        let config = StorageConfig::new(16 * 1024, 16 * 1024, 64, 1_024).unwrap();
        let storage = StructuralStorage::with_config(config).unwrap();
        let raw = AppendStore::with_memory_limit(1);
        let mut fixture = Vec::new();
        for _ in 0..40 {
            fixture.extend_from_slice(b"x\n");
        }
        raw.append(&fixture).unwrap();
        let store = DocumentLineStore::new(raw.clone(), &storage).unwrap();
        let spans = collect_spans(&store, &[&fixture]);
        assert_eq!(spans.len(), 40);
        for (id, span) in (0..40_u64).zip(spans) {
            assert_eq!(
                store
                    .append(
                        span,
                        LogicalLineMetadata {
                            commit_id: Some(id),
                            ..metadata(1, LineKind::CommitMetadata)
                        },
                    )
                    .unwrap(),
                id
            );
        }

        assert!(raw.is_spilled().unwrap());
        assert!(store.is_spilled().unwrap());
        let snapshot = store.snapshot().unwrap();
        let mut record_buffer = Vec::new();
        assert_eq!(
            snapshot
                .record_with_buffer(0, &mut record_buffer)
                .unwrap()
                .unwrap()
                .commit_id(),
            Some(0)
        );
        let pointer = record_buffer.as_ptr();
        let capacity = record_buffer.capacity();
        for id in 0..40_u64 {
            assert_eq!(
                snapshot
                    .record_with_buffer(id, &mut record_buffer)
                    .unwrap()
                    .unwrap()
                    .commit_id(),
                Some(id)
            );
            assert_eq!(record_buffer.as_ptr(), pointer);
            assert_eq!(record_buffer.capacity(), capacity);
        }
    }

    #[test]
    fn lazy_decode_matches_eager_ansi_crlf_invalid_utf8_and_unterminated_rules() {
        let raw = AppendStore::new();
        let ansi = b"\x1b[31m+hello\r\x1b[0m\n";
        let invalid = b"+bad:\xff\x00\n";
        let unterminated = b"tail\x1b[32m!\x1b[0m\xe0\xa0";
        let ansi_start = raw.append(ansi).unwrap();
        let invalid_start = raw.append(invalid).unwrap();
        let unterminated_start = raw.append(unterminated).unwrap();
        raw.seal().unwrap();
        let store = default_store(raw);
        let spans = collect_spans(&store, &[ansi, invalid, unterminated]);

        assert_eq!(spans[0].raw_start, ansi_start);
        assert_eq!(spans[1].raw_start, invalid_start);
        assert_eq!(spans[2].raw_start, unterminated_start);
        let mut spans = spans.into_iter();

        store
            .append(spans.next().unwrap(), metadata(6, LineKind::Addition))
            .unwrap();
        store
            .append(spans.next().unwrap(), metadata(13, LineKind::Addition))
            .unwrap();
        store
            .append(spans.next().unwrap(), metadata(13, LineKind::Plain))
            .unwrap();

        let snapshot = store.snapshot().unwrap();
        let mut decoder = LineDecoder::new();
        let owned = decoder
            .decode(&snapshot, 0)
            .unwrap()
            .unwrap()
            .try_to_owned()
            .unwrap();
        assert_eq!(owned.text, "+hello");
        assert_eq!(owned.content(), "hello");
        assert_eq!(
            decoder.decode(&snapshot, 1).unwrap().unwrap().text,
            "+bad:\\xFF\\x00"
        );
        assert_eq!(owned.as_borrowed().text, "+hello");
        assert_eq!(
            decoder.decode(&snapshot, 2).unwrap().unwrap().text,
            "tail!\\xE0\\xA0"
        );

        let mut eager_ansi = strip_ansi(&ansi[..ansi.len() - 1]);
        assert_eq!(eager_ansi.pop(), Some(b'\r'));
        assert_eq!(
            decoder.decode(&snapshot, 0).unwrap().unwrap().text,
            escape_interactive_bytes(&eager_ansi)
        );
    }

    #[test]
    fn lazy_decode_keeps_a_lone_cr_at_eof_visible() {
        let bytes = b"lone\r";
        let raw = AppendStore::new();
        raw.append(bytes).unwrap();
        raw.seal().unwrap();
        let store = default_store(raw);
        let span = collect_spans(&store, &[bytes]).into_iter().next().unwrap();
        store.append(span, metadata(8, LineKind::Plain)).unwrap();

        let snapshot = store.snapshot().unwrap();
        let mut decoder = LineDecoder::new();
        assert_eq!(
            decoder.decode(&snapshot, 0).unwrap().unwrap().text,
            "lone\\x0D"
        );
    }

    #[test]
    fn lazy_decode_ignores_raw_lf_inside_chunked_osc_and_dcs() {
        let chunks: [&[u8]; 4] = [
            b"\x1b]title\n",
            b"still\x07visible\n",
            b"\x1bPdata\n",
            b"more\x1b\\after\n",
        ];
        let raw = AppendStore::new();
        for chunk in chunks {
            raw.append(chunk).unwrap();
        }
        let store = default_store(raw);
        let spans = collect_spans(&store, &chunks);
        assert_eq!(spans.len(), 2);
        let mut spans = spans.into_iter();
        store
            .append(spans.next().unwrap(), metadata(7, LineKind::Plain))
            .unwrap();
        store
            .append(spans.next().unwrap(), metadata(5, LineKind::Plain))
            .unwrap();

        let snapshot = store.snapshot().unwrap();
        let mut decoder = LineDecoder::new();
        assert_eq!(
            decoder.decode(&snapshot, 0).unwrap().unwrap().text,
            "visible"
        );
        assert_eq!(decoder.decode(&snapshot, 1).unwrap().unwrap().text, "after");
    }

    #[test]
    fn lazy_decode_discards_unterminated_osc_and_dcs_at_eof() {
        for (bytes, expected) in [
            (b"prefix\x1b]title\ninside".as_slice(), "prefix"),
            (b"head\x1bPdata\ninside".as_slice(), "head"),
        ] {
            let raw = AppendStore::new();
            raw.append(bytes).unwrap();
            raw.seal().unwrap();
            let store = default_store(raw);
            let spans = collect_spans(&store, &[bytes]);
            assert_eq!(spans.len(), 1);
            assert!(!spans[0].terminated);
            let span = spans.into_iter().next().unwrap();
            store
                .append(span, metadata(expected.len() as u64, LineKind::Plain))
                .unwrap();

            let snapshot = store.snapshot().unwrap();
            let mut decoder = LineDecoder::new();
            assert_eq!(
                decoder.decode(&snapshot, 0).unwrap().unwrap().text,
                expected
            );
        }
    }

    #[test]
    fn decoder_rejects_incorrect_exact_and_overflow_display_lengths() {
        let raw = AppendStore::new();
        raw.append(b"x\nx\n").unwrap();
        let store = default_store(raw);
        let mut spans = collect_spans(&store, &[b"x\nx\n"]).into_iter();
        store
            .append(spans.next().unwrap(), metadata(2, LineKind::Plain))
            .unwrap();
        store
            .append(
                spans.next().unwrap(),
                LogicalLineMetadata {
                    display_len: u64::from(u32::MAX) + 1,
                    ..metadata(0, LineKind::Plain)
                },
            )
            .unwrap();

        let snapshot = store.snapshot().unwrap();
        let mut decoder = LineDecoder::new();
        assert!(matches!(
            decoder.decode(&snapshot, 0),
            Err(LineStoreError::DisplayLengthMismatch {
                stored: 2,
                overflow: false,
                actual: 1,
                ..
            })
        ));
        assert!(matches!(
            decoder.decode(&snapshot, 1),
            Err(LineStoreError::DisplayLengthMismatch {
                stored: u32::MAX,
                overflow: true,
                actual: 1,
                ..
            })
        ));
    }

    #[test]
    fn decoder_reuses_record_raw_stripped_and_text_buffers() {
        let config = StorageConfig::new(16 * 1024, 16 * 1024, 64, 64).unwrap();
        let storage = StructuralStorage::with_config(config).unwrap();
        let raw = AppendStore::with_memory_limit(1);
        let long = b"+0123456789abcdef\x1b[31mghij\x1b[0m\n";
        let short = b"+x\n";
        raw.append(long).unwrap();
        raw.append(short).unwrap();
        let store = DocumentLineStore::new(raw.clone(), &storage).unwrap();
        let mut spans = collect_spans(&store, &[long, short]).into_iter();
        store
            .append(spans.next().unwrap(), metadata(21, LineKind::Addition))
            .unwrap();
        store
            .append(spans.next().unwrap(), metadata(2, LineKind::Addition))
            .unwrap();

        assert!(raw.is_spilled().unwrap());
        assert!(store.is_spilled().unwrap());

        let snapshot = store.snapshot().unwrap();
        let mut decoder = LineDecoder::new();
        assert_eq!(
            decoder.decode(&snapshot, 0).unwrap().unwrap().text,
            "+0123456789abcdefghij"
        );
        let record_ptr = decoder.record_buffer.as_ptr();
        let raw_ptr = decoder.raw_buffer.as_ptr();
        let stripped_ptr = decoder.stripped_buffer.as_ptr();
        let text_ptr = decoder.text_buffer.as_ptr();
        let capacities = (
            decoder.record_buffer.capacity(),
            decoder.raw_buffer.capacity(),
            decoder.stripped_buffer.capacity(),
            decoder.text_buffer.capacity(),
        );

        assert_eq!(decoder.decode(&snapshot, 1).unwrap().unwrap().text, "+x");
        assert_eq!(decoder.record_buffer.as_ptr(), record_ptr);
        assert_eq!(decoder.raw_buffer.as_ptr(), raw_ptr);
        assert_eq!(decoder.stripped_buffer.as_ptr(), stripped_ptr);
        assert_eq!(decoder.text_buffer.as_ptr(), text_ptr);
        assert_eq!(
            (
                decoder.record_buffer.capacity(),
                decoder.raw_buffer.capacity(),
                decoder.stripped_buffer.capacity(),
                decoder.text_buffer.capacity(),
            ),
            capacities
        );
    }

    proptest! {
        #[test]
        fn chunked_indexed_tracker_records_match_eager_decode(
            input in prop::collection::vec(any::<u8>(), 0..1024),
            chunk_size in 1_usize..65,
        ) {
            let raw = AppendStore::new();
            raw.append(&input).unwrap();
            raw.seal().unwrap();
            let store = default_store(raw);
            let chunks: Vec<&[u8]> = input.chunks(chunk_size).collect();
            let spans = collect_spans(&store, &chunks);
            let expected = eager_plain_lines(&input);
            prop_assert_eq!(spans.len(), expected.len());

            for (span, text) in spans.into_iter().zip(&expected) {
                store
                    .append(
                        span,
                        metadata(u64::try_from(text.len()).unwrap(), LineKind::Plain),
                    )
                    .unwrap();
            }
            let snapshot = store.snapshot().unwrap();
            let mut decoder = LineDecoder::new();
            for (index, expected) in expected.iter().enumerate() {
                let line_id = u64::try_from(index).unwrap();
                let decoded = decoder.decode(&snapshot, line_id).unwrap().unwrap();
                prop_assert_eq!(decoded.text, expected.as_str());
            }
        }
    }
}
