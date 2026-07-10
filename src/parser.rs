//! Raw Git outputをstore-backed commit / file / hunk snapshotへ変換するincremental parser。
//!
//! Parserはreaderが[`AppendStore`]へcommit済みのchunkとabsolute offsetだけを受け取る。
//! [`AnsiStripper::push_indexed`]のcallbackでは表示byteとopaque raw-line spanだけを再利用bufferへ集め、
//! callback終了後にGit grammarを適用する。Line textは未完の末尾1行と解析中1行だけに置き、確定後は
//! [`DocumentBuilder`]がraw rangeと固定長metadataを保存する。
//!
//! 1行の構造更新後にrecord appendが失敗すると、open file countなどを巻き戻せない。最初のfailureで
//! parserをpoisonし、その後のpush/finish/snapshotを拒否するため、不整合なsnapshotは公開しない。
//! RepositoryやGit processは参照せず、stdinに現れたstandard headerだけをgrammarの根拠にする。

use std::mem;
use std::ops::Range;

use thiserror::Error;

use crate::ansi::{
    AnsiStripper, EscapeError, MAX_DEFERRED_OUTPUT_BYTES, RawOffsetOverflow,
    StreamingInteractiveEscaper, escape_path_bytes,
};
use crate::document::{AppendDocumentLine, DocumentBuilder, DocumentError, DocumentSnapshot};
use crate::document_store::{RawLineSpan, RawLineSpanError};
use crate::input::{AppendStore, StoreError};
use crate::model::{
    ChangeStatus, CommitId, CommitKind, FileId, FileMode, HunkId, HunkKind, LineId, LineKind,
    SourceRange,
};
use crate::storage::{StorageError, StructuralStorage};
use crate::text::MAX_SYNTAX_LINE_BYTES;

/// Git grammarの判定用に保持するescaped prefix上限。
///
/// Syntax対象の最大行と同じ1 MiBまでは従来どおり全文をgrammarへ渡す。それを越えたhunk bodyは
/// markerだけでsource counterを進め、header/path候補はpartial parseせずfallbackへ落とす。
const GRAMMAR_PREFIX_BYTES: usize = MAX_SYNTAX_LINE_BYTES;

/// Public `push` 1回のinputが巨大でもparser内部の作業bufferをこの単位で再利用する。
///
/// Readerのchunk設定はAPI callerごとに異なるため、public boundary自体が上限を所有する。
/// ANSI stateとunfinished logical lineはslice間で維持するので、grammar結果はcallerのchunkingに依存しない。
const MAX_PUSH_SLICE_BYTES: usize = 32 * 1024;

/// Raw event、ANSI boundary、document recordのどこでparseを継続できなくなったかを表すfailure。
#[derive(Debug, Error)]
pub enum ParseError {
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error(transparent)]
    RawStore(#[from] StoreError),
    #[error(transparent)]
    StructuralStorage(#[from] StorageError),
    #[error(transparent)]
    Escape(#[from] EscapeError),
    #[error(transparent)]
    RawOffsetOverflow(#[from] RawOffsetOverflow),
    #[error("raw chunk starts at {actual}, but parser expects offset {expected}")]
    NonContiguousRawChunk { expected: u64, actual: u64 },
    #[error("raw chunk range {start}..{end} exceeds the append store high-water mark {stored_len}")]
    RawChunkNotStored {
        start: u64,
        end: u64,
        stored_len: u64,
    },
    #[error("raw chunk bytes for {start}..{end} differ from the append store")]
    RawChunkMismatch { start: u64, end: u64 },
    #[error("raw input must be sealed before parser EOF is finalized")]
    RawInputNotSealed,
    #[error("parser consumed {parsed_len} raw bytes, but sealed input ends at {stored_len}")]
    RawLengthMismatch { parsed_len: u64, stored_len: u64 },
    #[error("raw logical-line tracking failed: {message}")]
    RawLineBoundary { message: String },
    #[error(
        "ANSI output contains {line_breaks} displayed line breaks but tracker produced {spans} spans"
    )]
    LineBoundaryMismatch { line_breaks: usize, spans: usize },
    #[error("EOF boundary tracker and pending displayed line disagree")]
    FinalLineBoundaryMismatch,
    #[error("could not allocate {requested} bytes for parser {buffer} buffer")]
    BufferAllocation {
        buffer: &'static str,
        requested: usize,
    },
    #[error("parser {buffer} buffer length overflow")]
    BufferLengthOverflow { buffer: &'static str },
    #[error("cannot push raw input after parser completion")]
    Finished,
    #[error("parser is poisoned by an earlier failure")]
    Poisoned,
}

impl From<RawLineSpanError> for ParseError {
    fn from(error: RawLineSpanError) -> Self {
        Self::RawLineBoundary {
            message: error.to_string(),
        }
    }
}

/// `push` / `finish` の1回で追加された append-only record 範囲。
///
/// renderer はこの range だけを index/cache へ反映し、既存 ID の再採番を行わない。status や
/// `updated_*` は呼び出し前から存在し、今回の chunk で field が変わり得る末尾 record を示す。
/// false positive を許す conservative invalidation なので、cache は append range の追加前に該当 ID
/// を invalidate し、[`GitParser::snapshot`] から読み直す。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseDelta {
    /// 今回appendされたlogical-line indexの半開範囲。
    pub lines: Range<usize>,
    /// 今回初めて可視になったcommitのstable-ID cursor範囲。
    pub commits: Range<u64>,
    /// 今回初めて可視になったfileのstable-ID cursor範囲。
    pub files: Range<u64>,
    /// 今回初めて可視になったhunkのstable-ID cursor範囲。
    pub hunks: Range<u64>,
    /// Push開始時にopenで、今回のfield更新対象になり得たcommit。
    pub updated_commit: Option<CommitId>,
    /// Push開始時にopenで、今回のfield更新対象になり得たfile。
    pub updated_file: Option<FileId>,
    /// Push開始時にopenで、今回のfield更新対象になり得たhunk。
    pub updated_hunk: Option<HunkId>,
    /// EOF処理とopen spanのcloseが完了した回だけ`true`。
    pub completed: bool,
}

#[derive(Clone, Copy, Debug)]
struct ParserSnapshot {
    lines: usize,
    commits: u64,
    files: u64,
    hunks: u64,
    open_commit: Option<CommitId>,
    open_file: Option<FileId>,
    open_hunk: Option<HunkId>,
}

impl ParserSnapshot {
    fn from_parser(parser: &GitParser) -> Result<Self, ParseError> {
        let counts = parser.document.structure_counts()?;
        Ok(Self {
            lines: parser.document.line_count(),
            commits: counts.commits,
            files: counts.files,
            hunks: counts.hunks,
            open_commit: parser.current_commit,
            open_file: parser.current_file,
            open_hunk: parser.current_hunk.map(|cursor| cursor.id),
        })
    }

    fn delta(
        self,
        document: &DocumentBuilder,
        completed: bool,
        may_update: bool,
    ) -> Result<ParseDelta, ParseError> {
        let counts = document.structure_counts()?;
        Ok(ParseDelta {
            lines: self.lines..document.line_count(),
            commits: self.commits..counts.commits,
            files: self.files..counts.files,
            hunks: self.hunks..counts.hunks,
            updated_commit: may_update.then_some(self.open_commit).flatten(),
            updated_file: may_update.then_some(self.open_file).flatten(),
            updated_hunk: may_update.then_some(self.open_hunk).flatten(),
            completed,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct HunkCursor {
    id: HunkId,
    old_next: u64,
    new_next: u64,
    old_remaining: u64,
    new_remaining: u64,
}

#[derive(Clone, Copy, Debug)]
struct RichHunkLine {
    kind: LineKind,
    old_line: Option<u64>,
    new_line: Option<u64>,
}

/// `Err` 側にも decode できた path を残し、malformed fallback の file identity に使う。
type ParsedPaths = Result<(Option<String>, Option<String>), (Option<String>, Option<String>)>;

/// Git byte stream を行単位で追記する parser。
///
/// `push`はreaderがraw storeへappendしたchunkのabsolute startを受け取り、ANSI stateとpartial lineを
/// chunk間で保持する。EOF eventを受けたcallerはraw storeをsealしてから[`GitParser::finish`]を呼ぶ。
/// `finish`は成功後に繰り返しても空deltaを返すが、完了後の`push`は[`ParseError::Finished`]となる。
#[derive(Debug)]
pub struct GitParser {
    raw: AppendStore,
    raw_cursor: u64,
    document: DocumentBuilder,
    ansi: AnsiStripper,
    tracker: crate::document_store::RawLineTracker,
    emitted: Vec<u8>,
    completed_spans: Vec<RawLineSpan>,
    stored_chunk: Vec<u8>,
    pending_line: BoundedLineAccumulator,
    current_commit: Option<CommitId>,
    current_file: Option<FileId>,
    current_hunk: Option<HunkCursor>,
    awaiting_new_file_header: bool,
    awaiting_hunk_header: bool,
    lifecycle: ParserLifecycle,
}

/// ANSI除去済みbyteを1 logical line分だけ数え、grammar prefixだけを保持する。
///
/// CRLF判定には除去後の末尾1 byteが必要なので、`last_byte`はline確定までescaperへ渡さない。
/// `prefix`は1 MiB、`escaped_chunk`は32 KiBの内部sliceの最大4倍に留まり、line全長には比例しない。
#[derive(Debug, Default)]
struct BoundedLineAccumulator {
    escaper: StreamingInteractiveEscaper,
    last_byte: Option<u8>,
    prefix: String,
    escaped_chunk: String,
    display_len: usize,
}

impl BoundedLineAccumulator {
    fn push_sanitized(&mut self, bytes: &[u8]) -> Result<(), ParseError> {
        if bytes.is_empty() {
            return Ok(());
        }
        if let Some(previous) = self.last_byte.take() {
            self.escape_part(&[previous])?;
        }
        if bytes.len() > 1 {
            self.escape_part(&bytes[..bytes.len() - 1])?;
        }
        self.last_byte = bytes.last().copied();
        Ok(())
    }

    fn finish(&mut self, terminated: bool) -> Result<(), ParseError> {
        if let Some(last) = self.last_byte.take()
            && !(terminated && last == b'\r')
        {
            self.escape_part(&[last])?;
        }
        self.escaped_chunk.clear();
        let _ = self.escaper.finish_into(&mut self.escaped_chunk)?;
        self.absorb_escaped()?;
        Ok(())
    }

    fn escape_part(&mut self, bytes: &[u8]) -> Result<(), ParseError> {
        self.escaped_chunk.clear();
        let _ = self.escaper.push_into(bytes, &mut self.escaped_chunk)?;
        self.absorb_escaped()
    }

    fn absorb_escaped(&mut self) -> Result<(), ParseError> {
        self.display_len = self
            .display_len
            .checked_add(self.escaped_chunk.len())
            .ok_or(ParseError::BufferLengthOverflow {
                buffer: "escaped line length",
            })?;
        let remaining = GRAMMAR_PREFIX_BYTES.saturating_sub(self.prefix.len());
        if remaining == 0 || self.escaped_chunk.is_empty() {
            return Ok(());
        }
        let mut end = remaining.min(self.escaped_chunk.len());
        while end > 0 && !self.escaped_chunk.is_char_boundary(end) {
            end -= 1;
        }
        if end > 0 {
            reserve_string_bytes(&mut self.prefix, end, "grammar prefix")?;
            self.prefix.push_str(&self.escaped_chunk[..end]);
        }
        Ok(())
    }

    fn is_oversized(&self) -> bool {
        self.display_len > GRAMMAR_PREFIX_BYTES
    }

    fn has_pending_input(&self) -> bool {
        self.last_byte.is_some() || self.display_len > 0 || !self.prefix.is_empty()
    }

    fn reset(&mut self) {
        self.escaper.reset();
        self.last_byte = None;
        self.prefix.clear();
        self.escaped_chunk.clear();
        self.display_len = 0;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParserLifecycle {
    Active,
    Finished,
    Poisoned,
}

impl GitParser {
    /// Raw storeとstructural budgetへ結び付いた空parserを作る。
    ///
    /// Store cloneをreaderがappendし、このparserには同じoffset/byte sliceを順番どおり渡す。Line tracker
    /// はstore identityごとに1つだけ発行されるため、同じbuilderから2つのparserは作れない。
    pub fn new(raw: AppendStore, storage: &StructuralStorage) -> Result<Self, ParseError> {
        let document = DocumentBuilder::new(raw.clone(), storage)?;
        let tracker = document.start_line_tracker()?;
        Ok(Self {
            raw,
            raw_cursor: 0,
            document,
            ansi: AnsiStripper::default(),
            tracker,
            emitted: Vec::new(),
            completed_spans: Vec::new(),
            stored_chunk: Vec::new(),
            pending_line: BoundedLineAccumulator::default(),
            current_commit: None,
            current_file: None,
            current_hunk: None,
            awaiting_new_file_header: false,
            awaiting_hunk_header: false,
            lifecycle: ParserLifecycle::Active,
        })
    }

    /// Storeへcommit済みの次chunkを取り込み、新しく追加されたrecord rangeを返す。
    ///
    /// 同じabsolute rangeをstoreから再読してbyte列の一致を検証する。Grammarとlazy decodeが別の入力を
    /// 根拠にするとtyped metadataを信用できないため、不一致はrecordを追加する前に拒否する。
    pub fn push(&mut self, raw_start: u64, input: &[u8]) -> Result<ParseDelta, ParseError> {
        match self.lifecycle {
            ParserLifecycle::Finished => return Err(ParseError::Finished),
            ParserLifecycle::Poisoned => return Err(ParseError::Poisoned),
            ParserLifecycle::Active => {}
        }
        let before = match ParserSnapshot::from_parser(self) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.lifecycle = ParserLifecycle::Poisoned;
                return Err(error);
            }
        };
        match self.push_active(raw_start, input) {
            Ok(()) => match before.delta(&self.document, false, !input.is_empty()) {
                Ok(delta) => Ok(delta),
                Err(error) => {
                    self.lifecycle = ParserLifecycle::Poisoned;
                    Err(error)
                }
            },
            Err(error) => {
                self.lifecycle = ParserLifecycle::Poisoned;
                Err(error)
            }
        }
    }

    /// EOF を確定し、unterminated final line と開いている span を閉じる。
    pub fn finish(&mut self) -> Result<ParseDelta, ParseError> {
        match self.lifecycle {
            ParserLifecycle::Finished => {
                let before = ParserSnapshot::from_parser(self)?;
                return before.delta(&self.document, true, false);
            }
            ParserLifecycle::Poisoned => return Err(ParseError::Poisoned),
            ParserLifecycle::Active => {}
        }
        let before = match ParserSnapshot::from_parser(self) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.lifecycle = ParserLifecycle::Poisoned;
                return Err(error);
            }
        };
        match self.finish_active() {
            Ok(()) => {
                self.lifecycle = ParserLifecycle::Finished;
                before.delta(&self.document, true, true)
            }
            Err(error) => {
                self.lifecycle = ParserLifecycle::Poisoned;
                Err(error)
            }
        }
    }

    /// 現在までにcommit済みのlineとstructural generationをcheap snapshotとして返す。
    pub fn snapshot(&self) -> Result<DocumentSnapshot, ParseError> {
        if self.lifecycle == ParserLifecycle::Poisoned {
            return Err(ParseError::Poisoned);
        }
        self.document.snapshot().map_err(ParseError::from)
    }

    /// EOF処理を完了し、最後のsnapshotを返す。Raw storeが未sealならfailureとなる。
    pub fn into_document(mut self) -> Result<DocumentSnapshot, ParseError> {
        let _ = self.finish()?;
        self.snapshot()
    }

    fn push_active(&mut self, raw_start: u64, input: &[u8]) -> Result<(), ParseError> {
        if raw_start != self.raw_cursor {
            return Err(ParseError::NonContiguousRawChunk {
                expected: self.raw_cursor,
                actual: raw_start,
            });
        }
        let input_len =
            u64::try_from(input.len()).map_err(|_| ParseError::BufferLengthOverflow {
                buffer: "raw chunk",
            })?;
        let raw_end = raw_start
            .checked_add(input_len)
            .ok_or(ParseError::RawOffsetOverflow(RawOffsetOverflow))?;
        let status = self.raw.status()?;
        if raw_end > status.len {
            return Err(ParseError::RawChunkNotStored {
                start: raw_start,
                end: raw_end,
                stored_len: status.len,
            });
        }
        let mut slice_start = raw_start;
        for slice in input.chunks(MAX_PUSH_SLICE_BYTES) {
            let slice_len =
                u64::try_from(slice.len()).map_err(|_| ParseError::BufferLengthOverflow {
                    buffer: "raw parser slice",
                })?;
            let slice_end = slice_start
                .checked_add(slice_len)
                .ok_or(ParseError::RawOffsetOverflow(RawOffsetOverflow))?;
            self.raw
                .read_range_into(slice_start..slice_end, &mut self.stored_chunk)?;
            if self.stored_chunk.as_slice() != slice {
                return Err(ParseError::RawChunkMismatch {
                    start: slice_start,
                    end: slice_end,
                });
            }

            self.collect_indexed(slice_start, slice)?;
            self.consume_collected()?;
            self.raw_cursor = slice_end;
            slice_start = slice_end;
        }
        debug_assert_eq!(slice_start, raw_end);
        Ok(())
    }

    fn finish_active(&mut self) -> Result<(), ParseError> {
        let status = self.raw.status()?;
        if !status.sealed {
            return Err(ParseError::RawInputNotSealed);
        }
        if status.len != self.raw_cursor {
            return Err(ParseError::RawLengthMismatch {
                parsed_len: self.raw_cursor,
                stored_len: status.len,
            });
        }

        self.collect_indexed_eof()?;
        self.consume_collected()?;
        let final_span = self.tracker.seal_eof(status.len)?;
        self.consume_final_span(final_span)?;

        let end = self.next_line_id()?;
        self.close_file(end)?;
        self.close_commit(end)?;
        self.document.mark_complete()?;
        Ok(())
    }

    /// Indexed callbackはgrammar stateへ触れず、事前確保済みbufferへbyte/spanを移すだけにする。
    fn collect_indexed(&mut self, raw_start: u64, input: &[u8]) -> Result<(), ParseError> {
        self.emitted.clear();
        self.completed_spans.clear();
        let emitted_bound = input.len().checked_add(MAX_DEFERRED_OUTPUT_BYTES).ok_or(
            ParseError::BufferLengthOverflow {
                buffer: "ANSI output",
            },
        )?;
        reserve_items(&mut self.emitted, emitted_bound, "ANSI output")?;
        let raw_line_breaks = input.iter().filter(|byte| **byte == b'\n').count();
        reserve_items(&mut self.completed_spans, raw_line_breaks, "raw line spans")?;

        let ansi = &mut self.ansi;
        let tracker = &mut self.tracker;
        let emitted = &mut self.emitted;
        let completed_spans = &mut self.completed_spans;
        let mut boundary_error = None;
        ansi.push_indexed(raw_start, input, |offset, byte| {
            emitted.push(byte);
            if boundary_error.is_some() {
                return;
            }
            match tracker.observe(offset, byte) {
                Ok(Some(span)) => completed_spans.push(span),
                Ok(None) => {}
                Err(error) => boundary_error = Some(error),
            }
        })?;
        if let Some(error) = boundary_error {
            return Err(error.into());
        }
        Ok(())
    }

    fn collect_indexed_eof(&mut self) -> Result<(), ParseError> {
        self.emitted.clear();
        self.completed_spans.clear();
        reserve_items(
            &mut self.emitted,
            MAX_DEFERRED_OUTPUT_BYTES,
            "ANSI EOF output",
        )?;
        reserve_items(
            &mut self.completed_spans,
            MAX_DEFERRED_OUTPUT_BYTES,
            "raw EOF line spans",
        )?;

        let tracker = &mut self.tracker;
        let emitted = &mut self.emitted;
        let completed_spans = &mut self.completed_spans;
        let mut boundary_error = None;
        self.ansi.finish_indexed(|offset, byte| {
            emitted.push(byte);
            if boundary_error.is_some() {
                return;
            }
            match tracker.observe(offset, byte) {
                Ok(Some(span)) => completed_spans.push(span),
                Ok(None) => {}
                Err(error) => boundary_error = Some(error),
            }
        });
        if let Some(error) = boundary_error {
            return Err(error.into());
        }
        Ok(())
    }

    fn consume_collected(&mut self) -> Result<(), ParseError> {
        let mut emitted = mem::take(&mut self.emitted);
        let mut spans = mem::take(&mut self.completed_spans);
        let mut pending = mem::take(&mut self.pending_line);

        let outcome = self.consume_buffers(&emitted, &mut spans, &mut pending);
        emitted.clear();
        spans.clear();
        self.emitted = emitted;
        self.completed_spans = spans;
        self.pending_line = pending;
        outcome
    }

    fn consume_buffers(
        &mut self,
        emitted: &[u8],
        spans: &mut Vec<RawLineSpan>,
        pending: &mut BoundedLineAccumulator,
    ) -> Result<(), ParseError> {
        let line_breaks = emitted.iter().filter(|byte| **byte == b'\n').count();
        if line_breaks != spans.len() {
            return Err(ParseError::LineBoundaryMismatch {
                line_breaks,
                spans: spans.len(),
            });
        }

        let mut start = 0_usize;
        for span in spans.drain(..) {
            let relative_end = emitted
                .get(start..)
                .and_then(|tail| tail.iter().position(|byte| *byte == b'\n'))
                .ok_or(ParseError::LineBoundaryMismatch {
                    line_breaks,
                    spans: line_breaks,
                })?;
            let end = start + relative_end;
            pending.push_sanitized(&emitted[start..end])?;
            pending.finish(true)?;
            self.process_accumulated_line(pending, span)?;
            pending.reset();
            start = end.saturating_add(1);
        }
        pending.push_sanitized(&emitted[start..])
    }

    fn consume_final_span(&mut self, span: Option<RawLineSpan>) -> Result<(), ParseError> {
        let mut pending = mem::take(&mut self.pending_line);
        let outcome = match (span, pending.has_pending_input()) {
            (None, false) => Ok(()),
            (Some(span), true) => {
                pending.finish(false)?;
                self.process_accumulated_line(&pending, span)
            }
            (None, true) | (Some(_), false) => Err(ParseError::FinalLineBoundaryMismatch),
        };
        if outcome.is_ok() {
            pending.reset();
        }
        self.pending_line = pending;
        outcome
    }

    fn process_accumulated_line(
        &mut self,
        pending: &BoundedLineAccumulator,
        span: RawLineSpan,
    ) -> Result<(), ParseError> {
        if pending.is_oversized() {
            self.process_oversized_line(&pending.prefix, pending.display_len, span)
        } else {
            debug_assert_eq!(pending.prefix.len(), pending.display_len);
            self.process_line(&pending.prefix, span)
        }
    }

    /// Grammar prefixを越えたlineを、全文を参照せずappendする。
    ///
    /// Unified hunk bodyは先頭markerとcursorだけで分類できるため、行番号・追加削除countを通常行と同じ
    /// ように進める。それ以外のfile内巨大行はpath/headerの末尾を見ずに解釈すると誤ったstructureを
    /// 作るためfileをMalformedへ落とす。File外ではcommit headerをpartial parseせずplain textとして
    /// current commitとの所有関係だけを維持する。
    fn process_oversized_line(
        &mut self,
        prefix: &str,
        display_len: usize,
        span: RawLineSpan,
    ) -> Result<(), ParseError> {
        if self.current_file.is_none() {
            return self.append_line(
                span,
                display_len,
                LineKind::Plain,
                self.current_commit,
                None,
                None,
                None,
                None,
            );
        }

        if self.current_hunk.is_some() {
            if self.current_file_mode()? == Some(FileMode::Rich) {
                if let Some(line) = self.consume_rich_hunk_line(prefix)? {
                    return self.append_current_file_line(
                        span,
                        display_len,
                        line.kind,
                        line.old_line,
                        line.new_line,
                    );
                }
                self.mark_current_file_mode(FileMode::Malformed)?;
                return self.append_current_file_line(
                    span,
                    display_len,
                    LineKind::Fallback,
                    None,
                    None,
                );
            }
            let kind = line_kind_for_mode(self.current_file_mode()?.unwrap_or(FileMode::Malformed));
            return self.append_current_file_line(span, display_len, kind, None, None);
        }

        self.awaiting_new_file_header = false;
        self.awaiting_hunk_header = false;
        self.mark_current_file_mode(FileMode::Malformed)?;
        self.append_current_file_line(span, display_len, LineKind::Fallback, None, None)
    }

    fn process_line(&mut self, text: &str, span: RawLineSpan) -> Result<(), ParseError> {
        let line_id = self.next_line_id()?;

        if let Some(object_id) = parse_commit_header(text) {
            self.close_file(line_id)?;
            self.close_commit(line_id)?;
            let commit_id =
                self.document
                    .begin_commit(CommitKind::Commit, Some(object_id), line_id)?;
            self.current_commit = Some(commit_id);
            return self.append_line(
                span,
                text.len(),
                LineKind::CommitHeader,
                Some(commit_id),
                None,
                None,
                None,
                None,
            );
        }

        // `--submodule=log` は環境によって `diff --git` を出さず、summary 行から始まる。
        // その場合も外側の submodule path を1 file occurrence として navigation 対象にする。
        if let Some(path) = parse_submodule_path(text) {
            let belongs_to_current = if let Some(id) = self.current_file {
                self.document
                    .open_file(id)?
                    .primary_path()
                    .is_some_and(|current| current == path)
            } else {
                false
            };
            if !belongs_to_current {
                self.start_file(
                    Ok((Some(path.clone()), Some(path))),
                    FileMode::Submodule,
                    line_id,
                )?;
            } else {
                self.mark_current_file_mode(FileMode::Submodule)?;
            }
            return self.append_current_file_line(
                span,
                text.len(),
                LineKind::Submodule,
                None,
                None,
            );
        }

        if let Some(rest) = text.strip_prefix("diff --git ") {
            let paths = parse_diff_git_paths(rest);
            if self.paths_are_inside_current_submodule(&paths)? {
                self.close_hunk(line_id)?;
                return self.append_current_file_line(
                    span,
                    text.len(),
                    LineKind::Submodule,
                    None,
                    None,
                );
            }
            self.start_file(paths, FileMode::Rich, line_id)?;
            return self.append_current_file_line(
                span,
                text.len(),
                LineKind::DiffHeader,
                None,
                None,
            );
        }

        if let Some(rest) = text
            .strip_prefix("diff --cc ")
            .or_else(|| text.strip_prefix("diff --combined "))
        {
            let path = decode_git_path(rest.trim());
            let paths = path
                .map(|value| (Some(value.clone()), Some(value)))
                .ok_or((None, None));
            self.start_file(paths, FileMode::Combined, line_id)?;
            return self.append_current_file_line(
                span,
                text.len(),
                LineKind::DiffHeader,
                None,
                None,
            );
        }

        if self.current_file.is_some() {
            self.process_file_line(text, span, line_id)
        } else {
            self.process_non_file_line(text, span)
        }
    }

    fn process_non_file_line(&mut self, text: &str, span: RawLineSpan) -> Result<(), ParseError> {
        let commit_id = self.current_commit;
        let kind = if let Some(id) = commit_id {
            if text.starts_with("    ") {
                let subject = text.trim();
                if !subject.is_empty() && self.document.open_commit(id)?.subject.is_none() {
                    self.document.open_commit_mut(id)?.subject = Some(subject.to_owned());
                }
                LineKind::CommitMessage
            } else {
                LineKind::CommitMetadata
            }
        } else {
            LineKind::Plain
        };

        self.append_line(span, text.len(), kind, commit_id, None, None, None, None)
    }

    fn process_file_line(
        &mut self,
        text: &str,
        span: RawLineSpan,
        line_id: LineId,
    ) -> Result<(), ParseError> {
        if self.awaiting_new_file_header && !text.starts_with("+++ ") {
            self.mark_current_file_mode(FileMode::Malformed)?;
            self.awaiting_new_file_header = false;
        }
        if self.awaiting_hunk_header && !text.starts_with("@@") && !is_binary_line(text) {
            self.mark_current_file_mode(FileMode::Malformed)?;
            self.awaiting_hunk_header = false;
        }

        if text.starts_with("@@@") {
            self.awaiting_hunk_header = false;
            self.close_hunk(line_id)?;
            self.mark_current_file_mode(FileMode::Combined)?;
            self.start_fallback_hunk(line_id, HunkKind::Combined)?;
            return self.append_current_file_line(
                span,
                text.len(),
                LineKind::HunkHeader,
                None,
                None,
            );
        }

        if text.starts_with("@@") {
            self.awaiting_hunk_header = false;
            self.close_hunk(line_id)?;
            if let Some((old_range, new_range)) = parse_unified_hunk_header(text) {
                let mode = self.current_file_mode()?.unwrap_or(FileMode::Malformed);
                if mode.is_rich() {
                    self.start_unified_hunk(line_id, old_range, new_range)?;
                } else {
                    self.start_fallback_hunk(line_id, hunk_kind_for_mode(mode))?;
                }
            } else {
                self.mark_current_file_mode(FileMode::Malformed)?;
                self.start_fallback_hunk(line_id, HunkKind::Malformed)?;
            }
            return self.append_current_file_line(
                span,
                text.len(),
                LineKind::HunkHeader,
                None,
                None,
            );
        }

        if text == "\\ No newline at end of file" {
            return self.append_current_file_line(
                span,
                text.len(),
                LineKind::NoNewlineMarker,
                None,
                None,
            );
        }

        if is_binary_line(text) {
            self.awaiting_hunk_header = false;
            self.mark_current_file_mode(FileMode::Binary)?;
            return self.append_current_file_line(span, text.len(), LineKind::Binary, None, None);
        }

        if self.current_hunk.is_some() {
            if self.current_file_mode()? == Some(FileMode::Rich) {
                if let Some(line) = self.consume_rich_hunk_line(text)? {
                    return self.append_current_file_line(
                        span,
                        text.len(),
                        line.kind,
                        line.old_line,
                        line.new_line,
                    );
                } else if is_word_diff_line(text) {
                    self.mark_current_file_mode(FileMode::WordDiff)?;
                    return self.append_current_file_line(
                        span,
                        text.len(),
                        LineKind::Fallback,
                        None,
                        None,
                    );
                } else if is_submodule_line(text) {
                    self.mark_current_file_mode(FileMode::Submodule)?;
                    return self.append_current_file_line(
                        span,
                        text.len(),
                        LineKind::Submodule,
                        None,
                        None,
                    );
                } else {
                    self.mark_current_file_mode(FileMode::Malformed)?;
                    return self.append_current_file_line(
                        span,
                        text.len(),
                        LineKind::Fallback,
                        None,
                        None,
                    );
                }
            } else {
                let kind =
                    line_kind_for_mode(self.current_file_mode()?.unwrap_or(FileMode::Malformed));
                return self.append_current_file_line(span, text.len(), kind, None, None);
            }
        }

        // inline submodule diff の `index` / `---` / `+++` は外側 file の path/status を
        // 上書きしてはならない。hunk header は上で navigation anchor として記録済みである。
        if self.current_file_mode()? == Some(FileMode::Submodule) {
            return self.append_current_file_line(
                span,
                text.len(),
                LineKind::Submodule,
                None,
                None,
            );
        }

        if is_gitlink_metadata(text) {
            self.mark_current_file_mode(FileMode::Submodule)?;
            return self.append_current_file_line(
                span,
                text.len(),
                LineKind::FileMetadata,
                None,
                None,
            );
        }

        if let Some(path) = text.strip_prefix("rename from ") {
            self.update_extended_path(path, true, ChangeStatus::Renamed)?;
            return self.append_current_file_line(
                span,
                text.len(),
                LineKind::FileMetadata,
                None,
                None,
            );
        }
        if let Some(path) = text.strip_prefix("rename to ") {
            self.update_extended_path(path, false, ChangeStatus::Renamed)?;
            return self.append_current_file_line(
                span,
                text.len(),
                LineKind::FileMetadata,
                None,
                None,
            );
        }
        if let Some(path) = text.strip_prefix("copy from ") {
            self.update_extended_path(path, true, ChangeStatus::Copied)?;
            return self.append_current_file_line(
                span,
                text.len(),
                LineKind::FileMetadata,
                None,
                None,
            );
        }
        if let Some(path) = text.strip_prefix("copy to ") {
            self.update_extended_path(path, false, ChangeStatus::Copied)?;
            return self.append_current_file_line(
                span,
                text.len(),
                LineKind::FileMetadata,
                None,
                None,
            );
        }
        if text.starts_with("new file mode ") {
            self.update_status(ChangeStatus::Added)?;
            return self.append_current_file_line(
                span,
                text.len(),
                LineKind::FileMetadata,
                None,
                None,
            );
        }
        if text.starts_with("deleted file mode ") {
            self.update_status(ChangeStatus::Deleted)?;
            return self.append_current_file_line(
                span,
                text.len(),
                LineKind::FileMetadata,
                None,
                None,
            );
        }
        if let Some(path) = text.strip_prefix("--- ") {
            self.awaiting_new_file_header = true;
            self.update_source_path(path, true)?;
            return self.append_current_file_line(
                span,
                text.len(),
                LineKind::FileMetadata,
                None,
                None,
            );
        }
        if let Some(path) = text.strip_prefix("+++ ") {
            if !self.awaiting_new_file_header {
                self.mark_current_file_mode(FileMode::Malformed)?;
            }
            self.awaiting_new_file_header = false;
            self.awaiting_hunk_header = true;
            self.update_source_path(path, false)?;
            return self.append_current_file_line(
                span,
                text.len(),
                LineKind::FileMetadata,
                None,
                None,
            );
        }

        let kind = line_kind_for_mode(self.current_file_mode()?.unwrap_or(FileMode::Malformed));
        self.append_current_file_line(span, text.len(), kind, None, None)
    }

    fn consume_rich_hunk_line(&mut self, text: &str) -> Result<Option<RichHunkLine>, ParseError> {
        let Some(marker) = text.as_bytes().first().copied() else {
            return Ok(None);
        };
        let Some(cursor) = self.current_hunk else {
            return Ok(None);
        };

        let (kind, old_line, new_line) = match marker {
            b' ' if cursor.old_remaining > 0 && cursor.new_remaining > 0 => (
                LineKind::Context,
                Some(cursor.old_next),
                Some(cursor.new_next),
            ),
            b'-' if cursor.old_remaining > 0 => (LineKind::Deletion, Some(cursor.old_next), None),
            b'+' if cursor.new_remaining > 0 => (LineKind::Addition, None, Some(cursor.new_next)),
            _ => return Ok(None),
        };

        if let Some(active) = self.current_hunk.as_mut() {
            match kind {
                LineKind::Context => {
                    active.old_next = active.old_next.saturating_add(1);
                    active.new_next = active.new_next.saturating_add(1);
                    active.old_remaining = active.old_remaining.saturating_sub(1);
                    active.new_remaining = active.new_remaining.saturating_sub(1);
                }
                LineKind::Deletion => {
                    active.old_next = active.old_next.saturating_add(1);
                    active.old_remaining = active.old_remaining.saturating_sub(1);
                }
                LineKind::Addition => {
                    active.new_next = active.new_next.saturating_add(1);
                    active.new_remaining = active.new_remaining.saturating_sub(1);
                }
                _ => {}
            }
        }

        if let Some(file_id) = self.current_file {
            let file = self.document.open_file_mut(file_id)?;
            match kind {
                LineKind::Deletion => {
                    file.deletions = file.deletions.map(|value| value.saturating_add(1));
                }
                LineKind::Addition => {
                    file.additions = file.additions.map(|value| value.saturating_add(1));
                }
                _ => {}
            }
        }

        Ok(Some(RichHunkLine {
            kind,
            old_line,
            new_line,
        }))
    }

    fn start_file(
        &mut self,
        paths: ParsedPaths,
        requested_mode: FileMode,
        line_id: LineId,
    ) -> Result<(), ParseError> {
        self.close_file(line_id)?;
        let commit_id = self.ensure_working_commit(line_id)?;
        let (old_path, new_path, mode) = match paths {
            Ok((old_path, new_path)) => (old_path, new_path, requested_mode),
            Err((old_path, new_path)) => (old_path, new_path, FileMode::Malformed),
        };
        let status = match mode {
            FileMode::Binary => ChangeStatus::Binary,
            FileMode::Submodule => ChangeStatus::Submodule,
            _ => ChangeStatus::Modified,
        };
        let file_id = self
            .document
            .begin_file(commit_id, old_path, new_path, status, mode, line_id)?;
        self.current_file = Some(file_id);
        self.awaiting_new_file_header = false;
        self.awaiting_hunk_header = false;
        Ok(())
    }

    fn ensure_working_commit(&mut self, line_id: LineId) -> Result<CommitId, ParseError> {
        if let Some(id) = self.current_commit {
            let _ = self.document.open_commit(id)?;
            return Ok(id);
        }

        let id = self
            .document
            .begin_commit(CommitKind::WorkingTree, None, line_id)?;
        self.current_commit = Some(id);
        Ok(id)
    }

    fn start_unified_hunk(
        &mut self,
        line_id: LineId,
        old_range: SourceRange,
        new_range: SourceRange,
    ) -> Result<(), ParseError> {
        let Some(file_id) = self.current_file else {
            return Ok(());
        };
        let hunk_id = self.document.begin_hunk(
            file_id,
            HunkKind::Unified,
            line_id,
            Some(old_range),
            Some(new_range),
        )?;
        self.current_hunk = Some(HunkCursor {
            id: hunk_id,
            old_next: old_range.start,
            new_next: new_range.start,
            old_remaining: old_range.count,
            new_remaining: new_range.count,
        });
        Ok(())
    }

    fn start_fallback_hunk(&mut self, line_id: LineId, kind: HunkKind) -> Result<(), ParseError> {
        let Some(file_id) = self.current_file else {
            return Ok(());
        };
        let hunk_id = self
            .document
            .begin_hunk(file_id, kind, line_id, None, None)?;
        self.current_hunk = Some(HunkCursor {
            id: hunk_id,
            old_next: 0,
            new_next: 0,
            old_remaining: 0,
            new_remaining: 0,
        });
        Ok(())
    }

    fn close_hunk(&mut self, end: LineId) -> Result<(), ParseError> {
        let Some(cursor) = self.current_hunk else {
            return Ok(());
        };
        let incomplete = self.current_file_mode()? == Some(FileMode::Rich)
            && (cursor.old_remaining != 0 || cursor.new_remaining != 0);
        if incomplete {
            self.mark_current_file_mode(FileMode::Malformed)?;
        }
        self.document.finish_hunk(cursor.id, end.get())?;
        // Fixed table appendが成功するまでIDを残す。Failure時はparserがpoisonされるが、
        // errorの手前でopen overlayへの参照を失わない。
        self.current_hunk = None;
        Ok(())
    }

    fn close_file(&mut self, end: LineId) -> Result<(), ParseError> {
        self.close_hunk(end)?;
        if self.awaiting_new_file_header {
            self.mark_current_file_mode(FileMode::Malformed)?;
            self.awaiting_new_file_header = false;
        }
        if self.awaiting_hunk_header {
            self.mark_current_file_mode(FileMode::Malformed)?;
            self.awaiting_hunk_header = false;
        }
        if let Some(file_id) = self.current_file {
            self.document.finish_file(file_id, end.get())?;
            self.current_file = None;
        }
        Ok(())
    }

    fn close_commit(&mut self, end: LineId) -> Result<(), ParseError> {
        if let Some(commit_id) = self.current_commit {
            self.document.finish_commit(commit_id, end.get())?;
            self.current_commit = None;
        }
        Ok(())
    }

    fn mark_current_file_mode(&mut self, mode: FileMode) -> Result<(), ParseError> {
        let Some(file_id) = self.current_file else {
            return Ok(());
        };
        {
            let file = self.document.open_file_mut(file_id)?;
            file.mode = mode;
            file.additions = None;
            file.deletions = None;
            if mode == FileMode::Binary {
                file.status = ChangeStatus::Binary;
            } else if mode == FileMode::Submodule {
                file.status = ChangeStatus::Submodule;
            }
        }
        if let Some(cursor) = self.current_hunk {
            let hunk = self.document.open_hunk_mut(cursor.id)?;
            hunk.kind = hunk_kind_for_mode(mode);
            if !mode.is_rich() {
                hunk.old_range = None;
                hunk.new_range = None;
            }
        }
        Ok(())
    }

    fn current_file_mode(&self) -> Result<Option<FileMode>, ParseError> {
        self.current_file
            .map(|file_id| self.document.open_file(file_id).map(|file| file.mode))
            .transpose()
            .map_err(ParseError::from)
    }

    fn paths_are_inside_current_submodule(&self, paths: &ParsedPaths) -> Result<bool, ParseError> {
        let Some(file_id) = self.current_file else {
            return Ok(false);
        };
        let file = self.document.open_file(file_id)?;
        if file.mode != FileMode::Submodule {
            return Ok(false);
        }
        let Some(root) = file.primary_path() else {
            return Ok(false);
        };
        let (old_path, new_path) = match paths {
            Ok(paths) | Err(paths) => paths,
        };
        Ok(old_path
            .as_deref()
            .into_iter()
            .chain(new_path.as_deref())
            .any(|path| {
                path.strip_prefix(root)
                    .is_some_and(|suffix| suffix.starts_with('/'))
            }))
    }

    fn update_status(&mut self, status: ChangeStatus) -> Result<(), ParseError> {
        if let Some(file_id) = self.current_file {
            self.document.open_file_mut(file_id)?.status = status;
        }
        Ok(())
    }

    fn update_extended_path(
        &mut self,
        value: &str,
        old: bool,
        status: ChangeStatus,
    ) -> Result<(), ParseError> {
        let decoded = decode_git_path(value.trim());
        let malformed = decoded.is_none();
        let Some(file_id) = self.current_file else {
            return Ok(());
        };
        {
            let file = self.document.open_file_mut(file_id)?;
            file.status = status;
            if let Some(decoded) = decoded {
                if old {
                    file.old_path = Some(decoded);
                } else {
                    file.new_path = Some(decoded);
                }
            }
        }
        if malformed {
            self.mark_current_file_mode(FileMode::Malformed)?;
        }
        Ok(())
    }

    fn update_source_path(&mut self, value: &str, old: bool) -> Result<(), ParseError> {
        // Git の `---` / `+++` header は timestamp field を持たないため、unquoted space も path
        // の一部として扱う。quoted path では decoder が閉じ quote 外の byte を拒否する。
        let token = value.trim();
        let decoded = decode_git_path_bytes(token);
        let is_dev_null = decoded.as_deref() == Some(b"/dev/null".as_slice());
        let display = decoded.as_deref().map(|bytes| {
            let without_prefix = if old {
                bytes.strip_prefix(b"a/").unwrap_or(bytes)
            } else {
                bytes.strip_prefix(b"b/").unwrap_or(bytes)
            };
            escape_path_bytes(without_prefix)
        });

        let Some(file_id) = self.current_file else {
            return Ok(());
        };
        {
            let file = self.document.open_file_mut(file_id)?;
            if old {
                if is_dev_null {
                    file.old_path = None;
                    file.status = ChangeStatus::Added;
                } else if display.is_some() {
                    file.old_path = display;
                }
            } else if is_dev_null {
                file.new_path = None;
                file.status = ChangeStatus::Deleted;
            } else if display.is_some() {
                file.new_path = display;
            }
        }
        if decoded.is_none() {
            self.mark_current_file_mode(FileMode::Malformed)?;
        }
        Ok(())
    }

    fn append_current_file_line(
        &mut self,
        span: RawLineSpan,
        display_len: usize,
        kind: LineKind,
        old_line_number: Option<u64>,
        new_line_number: Option<u64>,
    ) -> Result<(), ParseError> {
        let file_id = self.current_file;
        let commit_id = file_id
            .map(|id| self.document.open_file(id).map(|file| file.commit_id()))
            .transpose()?;
        let hunk_id = self.current_hunk.map(|cursor| cursor.id);
        self.append_line(
            span,
            display_len,
            kind,
            commit_id,
            file_id,
            hunk_id,
            old_line_number,
            new_line_number,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn append_line(
        &mut self,
        span: RawLineSpan,
        display_len: usize,
        kind: LineKind,
        commit_id: Option<CommitId>,
        file_id: Option<FileId>,
        hunk_id: Option<HunkId>,
        old_line_number: Option<u64>,
        new_line_number: Option<u64>,
    ) -> Result<(), ParseError> {
        let _ = self.document.append_line(
            span,
            AppendDocumentLine {
                display_len,
                kind,
                commit_id,
                file_id,
                hunk_id,
                old_line_number,
                new_line_number,
            },
        )?;
        Ok(())
    }

    fn next_line_id(&self) -> Result<LineId, ParseError> {
        self.document.next_line_id().map_err(ParseError::from)
    }
}

fn reserve_items<T>(
    buffer: &mut Vec<T>,
    requested: usize,
    name: &'static str,
) -> Result<(), ParseError> {
    buffer
        .try_reserve(requested)
        .map_err(|_| ParseError::BufferAllocation {
            buffer: name,
            requested,
        })
}

fn reserve_string_bytes(
    buffer: &mut String,
    additional: usize,
    name: &'static str,
) -> Result<(), ParseError> {
    let requested = buffer
        .len()
        .checked_add(additional)
        .ok_or(ParseError::BufferLengthOverflow { buffer: name })?;
    buffer
        .try_reserve(additional)
        .map_err(|_| ParseError::BufferAllocation {
            buffer: name,
            requested,
        })
}

/// Byte slice全体を独立raw storeへappendし、EOF済みsnapshotとして返す。
pub fn parse_bytes(input: &[u8]) -> Result<DocumentSnapshot, ParseError> {
    let raw = AppendStore::new();
    let storage = StructuralStorage::new()?;
    let mut parser = GitParser::new(raw.clone(), &storage)?;
    for chunk in input.chunks(MAX_PUSH_SLICE_BYTES) {
        let raw_start = raw.append(chunk)?;
        let _ = parser.push(raw_start, chunk)?;
    }
    let _ = raw.seal()?;
    parser.into_document()
}

/// Git の1つの path token を C-style quote / octal escape から表示 text へ戻す。
///
/// quoted token は閉じ quote の外側を受け付けない。unquoted path は field 全体として扱う。
/// tab、newline、invalid UTF-8 は path layout を壊さない `\\xNN` 表記になる。
#[must_use]
pub fn decode_git_path(token: &str) -> Option<String> {
    decode_git_path_bytes(token).map(|bytes| escape_path_bytes(&bytes))
}

fn parse_commit_header(line: &str) -> Option<String> {
    let rest = line.strip_prefix("commit ")?;
    let object_id = rest.split_ascii_whitespace().next()?;
    if (7..=64).contains(&object_id.len()) && object_id.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Some(object_id.to_owned())
    } else {
        None
    }
}

fn parse_diff_git_paths(rest: &str) -> ParsedPaths {
    let Some((old_token, remaining)) = take_token(rest) else {
        return Err((None, None));
    };
    let old_path = decode_diff_path(old_token, b"a/");
    let Some((new_token, trailing)) = take_token(remaining) else {
        return Err((old_path, None));
    };
    let new_path = decode_diff_path(new_token, b"b/");
    let malformed = old_path.is_none() || new_path.is_none() || !trailing.trim().is_empty();
    if malformed {
        Err((old_path, new_path))
    } else {
        Ok((old_path, new_path))
    }
}

fn decode_diff_path(token: &str, prefix: &[u8]) -> Option<String> {
    decode_git_path_bytes(token).map(|bytes| {
        let path = bytes.strip_prefix(prefix).unwrap_or(&bytes);
        escape_path_bytes(path)
    })
}

fn take_token(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start_matches([' ', '\t']);
    if input.is_empty() {
        return None;
    }
    if input.starts_with('"') {
        let bytes = input.as_bytes();
        let mut escaped = false;
        for (index, &byte) in bytes.iter().enumerate().skip(1) {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                let end = index.saturating_add(1);
                return Some((&input[..end], &input[end..]));
            }
        }
        None
    } else {
        let end = input.find([' ', '\t']).unwrap_or(input.len());
        Some((&input[..end], &input[end..]))
    }
}

fn decode_git_path_bytes(token: &str) -> Option<Vec<u8>> {
    if !token.starts_with('"') {
        return Some(token.as_bytes().to_vec());
    }
    if !token.ends_with('"') || token.len() < 2 {
        return None;
    }

    let inner = &token.as_bytes()[1..token.len().saturating_sub(1)];
    let mut output = Vec::with_capacity(inner.len());
    let mut index = 0;
    while index < inner.len() {
        if inner[index] != b'\\' {
            if inner[index] == b'"' {
                return None;
            }
            output.push(inner[index]);
            index = index.saturating_add(1);
            continue;
        }

        index = index.saturating_add(1);
        let escaped = *inner.get(index)?;
        if (b'0'..=b'7').contains(&escaped) {
            let mut value = 0_u16;
            let mut digits = 0;
            while digits < 3 && index < inner.len() && (b'0'..=b'7').contains(&inner[index]) {
                value = value
                    .saturating_mul(8)
                    .saturating_add(u16::from(inner[index] - b'0'));
                index = index.saturating_add(1);
                digits += 1;
            }
            if value > u16::from(u8::MAX) {
                return None;
            }
            output.push(value as u8);
            continue;
        }

        let decoded = match escaped {
            b'a' => 0x07,
            b'b' => 0x08,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 0x0b,
            b'f' => 0x0c,
            b'r' => b'\r',
            b'\\' => b'\\',
            b'"' => b'"',
            _ => return None,
        };
        output.push(decoded);
        index = index.saturating_add(1);
    }
    Some(output)
}

fn parse_unified_hunk_header(line: &str) -> Option<(SourceRange, SourceRange)> {
    let body = line.strip_prefix("@@ ")?;
    let end = body.find(" @@")?;
    let coordinates = &body[..end];
    let mut fields = coordinates.split_ascii_whitespace();
    let old = parse_source_range(fields.next()?, b'-')?;
    let new = parse_source_range(fields.next()?, b'+')?;
    if fields.next().is_some() {
        return None;
    }
    Some((old, new))
}

fn parse_source_range(value: &str, marker: u8) -> Option<SourceRange> {
    let bytes = value.as_bytes();
    if bytes.first().copied() != Some(marker) {
        return None;
    }
    let value = value.get(1..)?;
    let (start, count): (u64, u64) = match value.split_once(',') {
        Some((start, count)) => (parse_decimal(start)?, parse_decimal(count)?),
        None => (parse_decimal(value)?, 1),
    };
    if start == 0 && count > 0 {
        return None;
    }
    if count > 0 {
        let last = start.checked_add(count.saturating_sub(1))?;
        // Fixed line metadataは`u64::MAX`を`None` sentinelに使う。最後に発行する番号がsentinelへ
        // 到達するrangeはrich lineとして保存できないため、header全体をmalformed fallbackへ送る。
        if last == u64::MAX {
            return None;
        }
    }
    Some(SourceRange { start, count })
}

fn parse_decimal(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn is_binary_line(line: &str) -> bool {
    line == "GIT binary patch"
        || line.starts_with("Binary files ")
        || line.starts_with("literal ")
        || line.starts_with("delta ")
}

fn is_submodule_line(line: &str) -> bool {
    let content = line
        .strip_prefix(['+', '-', ' '])
        .unwrap_or(line)
        .trim_start();
    content.starts_with("Subproject commit ") || content.starts_with("Submodule ")
}

fn is_gitlink_metadata(line: &str) -> bool {
    (line.starts_with("index ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode "))
        && line.ends_with(" 160000")
}

fn parse_submodule_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("Submodule ")?;
    if rest.starts_with('"') {
        let (token, suffix) = take_token(rest)?;
        if !is_submodule_summary_suffix(suffix) {
            return None;
        }
        return decode_git_path(token);
    }

    // path 自体に `..` や `contains` が含まれても、右側の object range / marker を優先する。
    // field 数に比例する pointer 配列は作らず、oversized line でも追加 memory を一定に保つ。
    let mut scan_from = 0;
    let mut marker_start = None;
    for field in rest.split_ascii_whitespace() {
        let relative = rest.get(scan_from..)?.find(field)?;
        let start = scan_from.saturating_add(relative);
        let is_contains_marker =
            field == "contains" && rest.get(start..).is_some_and(is_submodule_contains_suffix);
        if is_contains_marker || is_git_object_range(field) {
            marker_start = Some(start);
        }
        scan_from = start.saturating_add(field.len());
    }
    let path = rest.get(..marker_start?)?.trim_end();
    if path.is_empty() {
        return None;
    }
    Some(escape_path_bytes(path.as_bytes()))
}

fn is_submodule_summary_suffix(suffix: &str) -> bool {
    is_submodule_contains_suffix(suffix)
        || suffix
            .split_ascii_whitespace()
            .next()
            .is_some_and(is_git_object_range)
}

fn is_submodule_contains_suffix(suffix: &str) -> bool {
    matches!(
        suffix.trim(),
        "contains modified content" | "contains untracked content"
    )
}

fn is_git_object_range(field: &str) -> bool {
    let field = field.trim_end_matches(':');
    let Some((old, new)) = field.split_once("..") else {
        return false;
    };
    (7..=64).contains(&old.len())
        && (7..=64).contains(&new.len())
        && old.bytes().all(|byte| byte.is_ascii_hexdigit())
        && new.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_word_diff_line(line: &str) -> bool {
    line == "~"
        || line.contains("{+")
        || line.contains("+}")
        || line.contains("[-")
        || line.contains("-]")
}

fn hunk_kind_for_mode(mode: FileMode) -> HunkKind {
    match mode {
        FileMode::Rich => HunkKind::Unified,
        FileMode::Combined => HunkKind::Combined,
        FileMode::WordDiff => HunkKind::WordDiff,
        FileMode::Submodule => HunkKind::Submodule,
        FileMode::Binary | FileMode::Malformed => HunkKind::Malformed,
    }
}

fn line_kind_for_mode(mode: FileMode) -> LineKind {
    match mode {
        FileMode::Binary => LineKind::Binary,
        FileMode::Submodule => LineKind::Submodule,
        FileMode::Rich => LineKind::FileMetadata,
        FileMode::Combined | FileMode::WordDiff | FileMode::Malformed => LineKind::Fallback,
    }
}

#[cfg(test)]
mod bounded_line_tests {
    use super::*;
    use crate::document::{DocumentLineMetadataReader, DocumentLineReader};
    use crate::structure_store::StructureMetadataReader;

    fn push(parser: &mut GitParser, raw: &AppendStore, bytes: &[u8]) {
        let start = raw.append(bytes).unwrap();
        let _ = parser.push(start, bytes).unwrap();
    }

    #[test]
    fn sixty_four_mib_hunk_line_keeps_parser_buffers_bounded_and_advances_source_numbers() {
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let mut parser = GitParser::new(raw.clone(), &storage).unwrap();
        push(
            &mut parser,
            &raw,
            b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -0,0 +1,2 @@\n",
        );

        let chunk = vec![b'a'; 32 * 1024];
        push(&mut parser, &raw, b"+");
        for _ in 0..(64 * 1024 * 1024 / chunk.len()) {
            push(&mut parser, &raw, &chunk);
        }
        assert_eq!(parser.pending_line.prefix.len(), GRAMMAR_PREFIX_BYTES);
        assert!(parser.pending_line.prefix.capacity() <= GRAMMAR_PREFIX_BYTES * 2);
        assert!(parser.pending_line.escaped_chunk.capacity() <= chunk.len() * 8);
        assert!(parser.emitted.capacity() <= chunk.len() * 2);
        assert!(parser.stored_chunk.capacity() <= chunk.len() * 2);

        push(&mut parser, &raw, b"\n+next\n");
        raw.seal().unwrap();
        let snapshot = parser.into_document().unwrap();
        let mut metadata_reader = DocumentLineMetadataReader::new();
        let additions = (0..snapshot.line_count())
            .filter_map(|index| {
                let id = snapshot.line_id_at(index).unwrap();
                metadata_reader.read(&snapshot, id).unwrap()
            })
            .filter(|line| line.kind == LineKind::Addition)
            .collect::<Vec<_>>();

        assert_eq!(additions.len(), 2);
        assert_eq!(additions[0].new_line_number, Some(1));
        assert_eq!(additions[1].new_line_number, Some(2));
        let second = DocumentLineReader::new()
            .read_owned(&snapshot, additions[1].id)
            .unwrap()
            .unwrap();
        assert_eq!(second.text, "+next");
        let file_id = snapshot.structure().file_id_at(0).unwrap();
        let file = StructureMetadataReader::new()
            .read_file(snapshot.structure(), file_id)
            .unwrap()
            .unwrap();
        assert_eq!(file.additions, Some(2));
    }

    #[test]
    fn one_shot_multi_megabyte_push_uses_only_bounded_internal_slices() {
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let mut parser = GitParser::new(raw.clone(), &storage).unwrap();
        let mut input = vec![b'x'; 8 * 1024 * 1024];
        input.push(b'\n');
        let start = raw.append(&input).unwrap();

        let delta = parser.push(start, &input).unwrap();
        assert_eq!(delta.lines, 0..1);
        assert_eq!(parser.raw_cursor, input.len() as u64);
        assert!(parser.stored_chunk.capacity() <= MAX_PUSH_SLICE_BYTES * 2);
        assert!(parser.emitted.capacity() <= MAX_PUSH_SLICE_BYTES * 2);
        assert!(parser.completed_spans.capacity() <= MAX_PUSH_SLICE_BYTES * 2);
        assert!(parser.pending_line.escaped_chunk.capacity() <= MAX_PUSH_SLICE_BYTES * 8);
        assert!(parser.pending_line.prefix.capacity() <= GRAMMAR_PREFIX_BYTES * 2);

        raw.seal().unwrap();
        let snapshot = parser.into_document().unwrap();
        let metadata = DocumentLineMetadataReader::new()
            .read(&snapshot, snapshot.line_id_at(0).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(metadata.raw_len, input.len().saturating_sub(1) as u64);
        assert!(metadata.display_len.exceeds(GRAMMAR_PREFIX_BYTES));
    }

    #[test]
    fn failed_structural_closes_keep_parser_current_ids() {
        let storage = StructuralStorage::new().unwrap();

        let mut commit_parser =
            GitParser::new(AppendStore::new(), &storage).expect("commit parser");
        let commit_id = commit_parser
            .document
            .begin_commit(CommitKind::WorkingTree, None, LineId::new(0))
            .unwrap();
        commit_parser.current_commit = Some(commit_id);
        assert!(commit_parser.close_commit(LineId::new(1)).is_err());
        assert_eq!(commit_parser.current_commit, Some(commit_id));

        let mut file_parser = GitParser::new(AppendStore::new(), &storage).expect("file parser");
        let commit_id = file_parser
            .document
            .begin_commit(CommitKind::WorkingTree, None, LineId::new(0))
            .unwrap();
        let file_id = file_parser
            .document
            .begin_file(
                commit_id,
                Some("a".to_owned()),
                Some("a".to_owned()),
                ChangeStatus::Modified,
                FileMode::Malformed,
                LineId::new(0),
            )
            .unwrap();
        file_parser.current_commit = Some(commit_id);
        file_parser.current_file = Some(file_id);
        assert!(file_parser.close_file(LineId::new(1)).is_err());
        assert_eq!(file_parser.current_file, Some(file_id));

        let mut hunk_parser = GitParser::new(AppendStore::new(), &storage).expect("hunk parser");
        let commit_id = hunk_parser
            .document
            .begin_commit(CommitKind::WorkingTree, None, LineId::new(0))
            .unwrap();
        let file_id = hunk_parser
            .document
            .begin_file(
                commit_id,
                Some("a".to_owned()),
                Some("a".to_owned()),
                ChangeStatus::Modified,
                FileMode::Malformed,
                LineId::new(0),
            )
            .unwrap();
        let hunk_id = hunk_parser
            .document
            .begin_hunk(file_id, HunkKind::Malformed, LineId::new(0), None, None)
            .unwrap();
        hunk_parser.current_commit = Some(commit_id);
        hunk_parser.current_file = Some(file_id);
        hunk_parser.current_hunk = Some(HunkCursor {
            id: hunk_id,
            old_next: 0,
            new_next: 0,
            old_remaining: 0,
            new_remaining: 0,
        });
        assert!(hunk_parser.close_hunk(LineId::new(1)).is_err());
        assert_eq!(
            hunk_parser.current_hunk.map(|cursor| cursor.id),
            Some(hunk_id)
        );
    }
}
