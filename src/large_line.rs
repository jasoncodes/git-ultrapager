//! 1 MiBを越えるlogical lineを、raw storeからbounded bufferで逐次decodeする。
//!
//! [`LargeLineTextReader`]はANSI除去・invalid/control escape後のtextを通常のUnicode grapheme単位で
//! 返す。`GraphemeCursor`をchunk間で継続し、64 KiBを越えたclusterのprefixを捨てた後もsuffixを別の
//! graphemeとして再segmentしない。Fallback片は既存の`wrap_hard`と同じscalar境界で切る。EOFでは
//! 仮想control 1 byteで最後のboundaryを確定するため、u32に収まらない表示長も全文長を先に保持しない。
//! 保持量はraw/escaped chunk、最大1 fallback片、cursor stateに制限され、logical line全長に比例しない。

use std::ops::Range;

use thiserror::Error;
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};

use crate::document::{
    DocumentError, DocumentLineMetadata, DocumentLineMetadataReader, DocumentSnapshot,
};
use crate::document_store::{
    CHUNKED_LINE_RAW_BYTES, ChunkedLineDecoder, ChunkedLineDecoderStats, LineStoreError,
};
use crate::model::LineId;
use crate::text::MAX_GRAPHEME_BYTES;

/// 巨大行のdecode、stable ID、bounded buffer更新に失敗した理由。
#[derive(Debug, Error)]
pub enum LargeLineError {
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error(transparent)]
    LineStore(#[from] LineStoreError),
    #[error("document snapshot does not contain logical line {line_id}")]
    MissingLine { line_id: LineId },
    #[error("could not allocate {requested} bytes for large-line {buffer} buffer")]
    Allocation {
        buffer: &'static str,
        requested: usize,
    },
    #[error("large-line logical byte offset overflow")]
    LogicalOffsetOverflow,
    #[error("streaming grapheme cursor requested unavailable {request} context")]
    GraphemeCursor { request: &'static str },
}

/// DecoderのI/O量と同時に保持したtext payloadの最大値。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LargeLineStats {
    pub raw_bytes_read: u64,
    pub peak_buffered_bytes: usize,
}

/// Escape後logical textの1 grapheme、または64 KiB fallback片。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LargeTextUnit<'a> {
    pub text: &'a str,
    pub logical_range: Range<usize>,
}

/// 1回のbounded読取で生じた表示unit、raw progress、またはlogical EOF。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LargeLineReadStep<'a> {
    Unit(LargeTextUnit<'a>),
    /// 1 raw chunkを処理したが、grapheme boundaryまたは表示textはまだ確定していない。
    Progress,
    End,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ReadStepOutcome {
    Unit(Range<usize>),
    Progress,
    End,
}

/// Raw snapshotから1 logical lineを逐次読む再利用可能reader。
#[derive(Debug)]
pub struct LargeLineTextReader {
    decoder: ChunkedLineDecoder,
    metadata_reader: DocumentLineMetadataReader,
    pending: String,
    pending_start: usize,
    unit: String,
    logical_cursor: usize,
    visible_emitted: usize,
    skip_prefix_bytes: usize,
    grapheme_cursor: GraphemeCursor,
    pending_boundary: Option<usize>,
    logical_end: Option<usize>,
    peak_text_bytes: usize,
}

impl Default for LargeLineTextReader {
    fn default() -> Self {
        Self {
            decoder: ChunkedLineDecoder::new(),
            metadata_reader: DocumentLineMetadataReader::new(),
            pending: String::new(),
            pending_start: 0,
            unit: String::new(),
            logical_cursor: 0,
            visible_emitted: 0,
            skip_prefix_bytes: 0,
            // Decoder完了時に仮想改行を1 byteだけ追加し、実際の末尾をboundaryとして確定する。
            // これによりrecordがu32長をoverflowしても、全文長をcursor生成時に知る必要がない。
            grapheme_cursor: GraphemeCursor::new(0, usize::MAX, true),
            pending_boundary: None,
            logical_end: None,
            peak_text_bytes: 0,
        }
    }
}

impl LargeLineTextReader {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lineを先頭から開始する。
    ///
    /// `skip_prefix_bytes`はsource markerの1 byteをlayout対象から除く用途で使う。Skipしたbyteも
    /// logical offsetには含まれるため、最初のunit rangeはその値から始まる。
    pub fn start(
        &mut self,
        document: &DocumentSnapshot,
        line_id: LineId,
        skip_prefix_bytes: usize,
    ) -> Result<DocumentLineMetadata, LargeLineError> {
        let metadata = self
            .metadata_reader
            .read(document, line_id)?
            .ok_or(LargeLineError::MissingLine { line_id })?;
        let _ = self
            .decoder
            .start(document.line_snapshot(), line_id.get())?
            .ok_or(LargeLineError::MissingLine { line_id })?;
        self.pending.clear();
        self.pending_start = 0;
        self.unit.clear();
        self.logical_cursor = skip_prefix_bytes;
        self.visible_emitted = 0;
        self.skip_prefix_bytes = skip_prefix_bytes;
        self.grapheme_cursor = GraphemeCursor::new(0, usize::MAX, true);
        self.pending_boundary = None;
        self.logical_end = None;
        self.peak_text_bytes = 0;
        Ok(metadata)
    }

    /// 次のgrapheme/fallback片を返す。返り値は次回呼び出しまで有効。
    pub fn next_unit(&mut self) -> Result<Option<LargeTextUnit<'_>>, LargeLineError> {
        loop {
            match self.advance_step()? {
                ReadStepOutcome::Unit(logical_range) => {
                    return Ok(Some(LargeTextUnit {
                        text: &self.unit,
                        logical_range,
                    }));
                }
                ReadStepOutcome::Progress => {}
                ReadStepOutcome::End => return Ok(None),
            }
        }
    }

    /// Decoderを最大1 raw chunkだけ進める。
    ///
    /// [`LargeLineReadStep::Progress`]でcallerへ戻るため、表示textを出さない巨大OSC/DCS payloadでも
    /// 64 KiBごとにcancellationやcooperative schedulingを行える。
    pub fn next_step(&mut self) -> Result<LargeLineReadStep<'_>, LargeLineError> {
        match self.advance_step()? {
            ReadStepOutcome::Unit(logical_range) => Ok(LargeLineReadStep::Unit(LargeTextUnit {
                text: &self.unit,
                logical_range,
            })),
            ReadStepOutcome::Progress => Ok(LargeLineReadStep::Progress),
            ReadStepOutcome::End => Ok(LargeLineReadStep::End),
        }
    }

    fn advance_step(&mut self) -> Result<ReadStepOutcome, LargeLineError> {
        loop {
            self.compact_pending();
            if self.logical_end == Some(self.visible_emitted) {
                return Ok(ReadStepOutcome::End);
            }
            if let Some(boundary) = self.pending_boundary {
                let available = boundary.checked_sub(self.visible_emitted).ok_or(
                    LargeLineError::GraphemeCursor {
                        request: "non-monotonic boundary",
                    },
                )?;
                if available == 0 {
                    self.pending_boundary = None;
                    continue;
                }
                let end = if available > MAX_GRAPHEME_BYTES {
                    scalar_boundary_at_or_before(
                        &self.pending[self.pending_start..self.pending_start + available],
                        MAX_GRAPHEME_BYTES,
                    )
                    .ok_or(LargeLineError::GraphemeCursor {
                        request: "UTF-8 scalar boundary",
                    })?
                } else {
                    self.pending_boundary = None;
                    available
                };
                return self.emit_unit(end).map(ReadStepOutcome::Unit);
            }

            let remaining = &self.pending[self.pending_start..];
            if remaining.is_empty() {
                self.refill()?;
                return Ok(ReadStepOutcome::Progress);
            }
            match self
                .grapheme_cursor
                .next_boundary(remaining, self.visible_emitted)
            {
                Ok(Some(boundary)) => {
                    if self.logical_end.is_some_and(|end| boundary > end) {
                        return Err(LargeLineError::GraphemeCursor {
                            request: "boundary beyond logical line end",
                        });
                    }
                    self.pending_boundary = Some(boundary);
                }
                Ok(None) => {
                    return Err(LargeLineError::GraphemeCursor {
                        request: "finite cursor end",
                    });
                }
                Err(GraphemeIncomplete::NextChunk) => {
                    // Cursorはgrapheme stateを保持したままchunk末尾まで進んでいる。未確定clusterが
                    // 64 KiBへ達したらscalar境界で先頭片を返し、suffixを再segmentしない。
                    let visible_len = self.logical_end.map_or(remaining.len(), |end| {
                        end.saturating_sub(self.visible_emitted)
                    });
                    if visible_len >= MAX_GRAPHEME_BYTES {
                        let end = scalar_boundary_at_or_before(
                            &remaining[..visible_len],
                            MAX_GRAPHEME_BYTES,
                        )
                        .ok_or(LargeLineError::GraphemeCursor {
                            request: "UTF-8 scalar boundary",
                        })?;
                        return self.emit_unit(end).map(ReadStepOutcome::Unit);
                    }
                    self.refill()?;
                    return Ok(ReadStepOutcome::Progress);
                }
                Err(GraphemeIncomplete::PreContext(_)) => {
                    return Err(LargeLineError::GraphemeCursor {
                        request: "discarded pre-context",
                    });
                }
                Err(GraphemeIncomplete::PrevChunk) => {
                    return Err(LargeLineError::GraphemeCursor {
                        request: "previous chunk",
                    });
                }
                Err(GraphemeIncomplete::InvalidOffset) => {
                    return Err(LargeLineError::GraphemeCursor {
                        request: "valid chunk offset",
                    });
                }
            }
        }
    }

    #[must_use]
    pub fn stats(&self) -> LargeLineStats {
        let ChunkedLineDecoderStats {
            raw_bytes_read,
            peak_buffered_bytes,
        } = self.decoder.stats();
        LargeLineStats {
            raw_bytes_read,
            peak_buffered_bytes: peak_buffered_bytes.saturating_add(self.peak_text_bytes),
        }
    }

    fn refill(&mut self) -> Result<(), LargeLineError> {
        let Some(chunk) = self.decoder.next_chunk()? else {
            if self.logical_end.is_some() {
                return Err(LargeLineError::GraphemeCursor {
                    request: "input after EOF sentinel",
                });
            }
            let remaining = self.pending.len().saturating_sub(self.pending_start);
            let end = self
                .visible_emitted
                .checked_add(remaining)
                .ok_or(LargeLineError::LogicalOffsetOverflow)?;
            self.logical_end = Some(end);
            // GraphemeCursorはusize::MAXを仮の全文長として進む。EOFではcontrolを仮置きすると、
            // 直前clusterとの間がUAX #29のboundaryになり、control自体はunitへ含まれない。
            reserve_string(&mut self.pending, 1, "grapheme EOF sentinel")?;
            self.pending.push('\n');
            self.observe_peak();
            return Ok(());
        };
        let skip = self.skip_prefix_bytes.min(chunk.len());
        self.skip_prefix_bytes -= skip;
        let visible = &chunk[skip..];
        reserve_string(&mut self.pending, visible.len(), "grapheme carry")?;
        self.pending.push_str(visible);
        self.observe_peak();
        Ok(())
    }

    fn emit_unit(&mut self, end: usize) -> Result<Range<usize>, LargeLineError> {
        self.unit.clear();
        reserve_string(&mut self.unit, end, "unit")?;
        self.unit
            .push_str(&self.pending[self.pending_start..self.pending_start + end]);
        self.pending_start += end;
        self.visible_emitted = self
            .visible_emitted
            .checked_add(end)
            .ok_or(LargeLineError::LogicalOffsetOverflow)?;
        let start = self.logical_cursor;
        self.logical_cursor = self
            .logical_cursor
            .checked_add(end)
            .ok_or(LargeLineError::LogicalOffsetOverflow)?;
        self.observe_peak();
        Ok(start..self.logical_cursor)
    }

    fn compact_pending(&mut self) {
        if self.pending_start == 0 {
            return;
        }
        if self.pending_start == self.pending.len() || self.pending_start >= CHUNKED_LINE_RAW_BYTES
        {
            self.pending.drain(..self.pending_start);
            self.pending_start = 0;
        }
    }

    fn observe_peak(&mut self) {
        let buffered = self
            .pending
            .len()
            .saturating_sub(self.pending_start)
            .saturating_add(self.unit.len());
        self.peak_text_bytes = self.peak_text_bytes.max(buffered);
    }
}

fn scalar_boundary_at_or_before(text: &str, limit: usize) -> Option<usize> {
    let mut end = limit.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (end > 0).then_some(end)
}

fn reserve_string(
    buffer: &mut String,
    additional: usize,
    name: &'static str,
) -> Result<(), LargeLineError> {
    let requested = buffer
        .len()
        .checked_add(additional)
        .ok_or(LargeLineError::LogicalOffsetOverflow)?;
    buffer
        .try_reserve(additional)
        .map_err(|_| LargeLineError::Allocation {
            buffer: name,
            requested,
        })
}
