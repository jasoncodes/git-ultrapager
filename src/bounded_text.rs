//! Logical lineのmetadata判定と、上限付きtext復元を1つのreaderへまとめる。
//!
//! [`BoundedDocumentTextReader`]はfixed recordを先に読み、表示text長がcallerの上限を越えるlineを
//! raw storeへ触れずに[`BoundedTextRead::Oversized`]へ分類する。表示長は上限内でもraw payloadが
//! 固定のeager境界1 MiBを越える場合、[`LargeLineTextReader`]を使って64 KiBずつdecodeし、各stepの
//! 後でcallerのcancellation predicateを評価する。完成textだけを再利用bufferに保持し、raw payloadや
//! line全文に比例する一時bufferを追加しない。
//!
//! Readerは1 worker内で再利用する。`Available`のtextは次の`read`までだけ有効であり、別threadへ渡す
//! 場合はmatch spanなどの小さな結果へ変換してから返す。

use thiserror::Error;

use crate::document::{
    DocumentError, DocumentLineMetadata, DocumentLineMetadataReader, DocumentLineReader,
    DocumentSnapshot,
};
use crate::large_line::{LargeLineError, LargeLineReadStep, LargeLineStats, LargeLineTextReader};
use crate::model::LineId;

/// 上限付きline読取の完了状態。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundedTextRead<'text> {
    /// ANSI除去とcontrol-byte escapeを終え、recordの表示長と一致したtext。
    Available {
        metadata: DocumentLineMetadata,
        text: &'text str,
    },
    /// 表示text長がcallerの上限を越えたline。Raw rangeは読んでいない。
    Oversized { metadata: DocumentLineMetadata },
    /// Predicateがtrueになったため、途中までのtextを公開せず中断した。
    Cancelled,
}

/// Eager decoderへ渡すraw rangeの最大値。
///
/// Callerの表示上限が16 MiBでも、raw/ANSI除去/escaped textの複数bufferを同時に確保せず、1 MiBを
/// 越えたlineでは64 KiBごとにcancellationへ戻るための固定境界である。
pub const MAX_EAGER_TEXT_RAW_BYTES: usize = 1024 * 1024;

/// Metadata、raw decode、またはbounded output bufferの構築に失敗した理由。
#[derive(Debug, Error)]
pub enum BoundedTextError {
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error(transparent)]
    LargeLine(#[from] LargeLineError),
    #[error("document snapshot does not contain logical line {line_id}")]
    MissingLine { line_id: LineId },
    #[error("could not allocate {requested} bytes for bounded logical line {line_id}")]
    Allocation { line_id: LineId, requested: usize },
    #[error("bounded logical line {line_id} does not have an exact display length")]
    ExactDisplayLengthUnavailable { line_id: LineId },
    #[error("bounded logical line {line_id} metadata changed within one immutable snapshot read")]
    MetadataChanged { line_id: LineId },
    #[error(
        "bounded logical line {line_id} emitted non-contiguous range {actual_start}..{actual_end}; expected start {expected_start}"
    )]
    NonContiguousRange {
        line_id: LineId,
        expected_start: usize,
        actual_start: usize,
        actual_end: usize,
    },
    #[error(
        "bounded logical line {line_id} decoded {actual} display bytes, but metadata records {expected}"
    )]
    DisplayLengthMismatch {
        line_id: LineId,
        expected: usize,
        actual: usize,
    },
}

/// Metadata-first判定とeager/streaming decodeを共有する再利用可能reader。
#[derive(Debug)]
pub struct BoundedDocumentTextReader {
    metadata_reader: DocumentLineMetadataReader,
    eager_reader: DocumentLineReader,
    large_reader: LargeLineTextReader,
    streamed_text: String,
}

impl Default for BoundedDocumentTextReader {
    fn default() -> Self {
        Self {
            metadata_reader: DocumentLineMetadataReader::new(),
            eager_reader: DocumentLineReader::new(),
            large_reader: LargeLineTextReader::new(),
            streamed_text: String::new(),
        }
    }
}

impl BoundedDocumentTextReader {
    /// Allocation前の空bufferを持つreaderを作る。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 1 logical lineを表示byte上限内で復元する。
    ///
    /// `max_display_bytes`を越えるrecordはmetadataだけで`Oversized`になる。Raw長が固定の1 MiB以内なら
    /// eager decoderを使い、それを越えるraw rangeは64 KiB stepのstreaming decoderへ切り替える。
    /// Predicateはmetadata/raw I/Oの前、eager decodeの後、およびstreamingの各`Progress`/`Unit`後に
    /// 評価する。`Cancelled`では途中bufferのborrowを返さない。
    pub fn read<'reader, Cancel: Fn() -> bool + ?Sized>(
        &'reader mut self,
        document: &DocumentSnapshot,
        line_id: LineId,
        max_display_bytes: usize,
        cancelled: &Cancel,
    ) -> Result<BoundedTextRead<'reader>, BoundedTextError> {
        if cancelled() {
            return Ok(BoundedTextRead::Cancelled);
        }
        let metadata = self
            .metadata_reader
            .read(document, line_id)?
            .ok_or(BoundedTextError::MissingLine { line_id })?;
        if metadata.display_len.exceeds(max_display_bytes) {
            return Ok(BoundedTextRead::Oversized { metadata });
        }
        if cancelled() {
            return Ok(BoundedTextRead::Cancelled);
        }

        let raw_exceeds =
            u64::try_from(MAX_EAGER_TEXT_RAW_BYTES).map_or(true, |limit| metadata.raw_len > limit);
        if raw_exceeds {
            self.read_streaming(document, metadata, max_display_bytes, cancelled)
        } else {
            self.read_eager(document, metadata, cancelled)
        }
    }

    /// 直近のstreaming decode統計を返す。Eager読取後は以前のstreaming値を保持する。
    #[must_use]
    pub fn large_line_stats(&self) -> LargeLineStats {
        self.large_reader.stats()
    }

    fn read_eager<'reader, Cancel: Fn() -> bool + ?Sized>(
        &'reader mut self,
        document: &DocumentSnapshot,
        metadata: DocumentLineMetadata,
        cancelled: &Cancel,
    ) -> Result<BoundedTextRead<'reader>, BoundedTextError> {
        let line = self.eager_reader.read(document, metadata.id)?.ok_or(
            BoundedTextError::MissingLine {
                line_id: metadata.id,
            },
        )?;
        if cancelled() {
            return Ok(BoundedTextRead::Cancelled);
        }
        validate_display_length(metadata, line.text.len())?;
        Ok(BoundedTextRead::Available {
            metadata,
            text: line.text,
        })
    }

    fn read_streaming<'reader, Cancel: Fn() -> bool + ?Sized>(
        &'reader mut self,
        document: &DocumentSnapshot,
        metadata: DocumentLineMetadata,
        max_display_bytes: usize,
        cancelled: &Cancel,
    ) -> Result<BoundedTextRead<'reader>, BoundedTextError> {
        let expected = metadata.display_len.exact().ok_or(
            BoundedTextError::ExactDisplayLengthUnavailable {
                line_id: metadata.id,
            },
        )?;
        // `exceeds`判定後なので通常は成立する。Metadata codecの意味が変わった場合も、上限を越える
        // reserveへ進まずtyped mismatchとして止める。
        if expected > max_display_bytes {
            return Ok(BoundedTextRead::Oversized { metadata });
        }

        self.streamed_text.clear();
        if self.streamed_text.capacity() < expected {
            self.streamed_text
                .try_reserve_exact(expected)
                .map_err(|_| BoundedTextError::Allocation {
                    line_id: metadata.id,
                    requested: expected,
                })?;
        }
        let streamed_metadata = self.large_reader.start(document, metadata.id, 0)?;
        if streamed_metadata != metadata {
            return Err(BoundedTextError::MetadataChanged {
                line_id: metadata.id,
            });
        }

        loop {
            let step = self.large_reader.next_step()?;
            if cancelled() {
                return Ok(BoundedTextRead::Cancelled);
            }
            match step {
                LargeLineReadStep::Unit(unit) => {
                    let actual_start = unit.logical_range.start;
                    let actual_end = unit.logical_range.end;
                    if actual_start != self.streamed_text.len()
                        || actual_end != actual_start.saturating_add(unit.text.len())
                    {
                        return Err(BoundedTextError::NonContiguousRange {
                            line_id: metadata.id,
                            expected_start: self.streamed_text.len(),
                            actual_start,
                            actual_end,
                        });
                    }
                    let new_len = self
                        .streamed_text
                        .len()
                        .checked_add(unit.text.len())
                        .ok_or(BoundedTextError::DisplayLengthMismatch {
                            line_id: metadata.id,
                            expected,
                            actual: usize::MAX,
                        })?;
                    if new_len > expected || new_len > max_display_bytes {
                        return Err(BoundedTextError::DisplayLengthMismatch {
                            line_id: metadata.id,
                            expected,
                            actual: new_len,
                        });
                    }
                    self.streamed_text.push_str(unit.text);
                }
                LargeLineReadStep::Progress => {}
                LargeLineReadStep::End => break,
            }
        }

        validate_display_length(metadata, self.streamed_text.len())?;
        Ok(BoundedTextRead::Available {
            metadata,
            text: &self.streamed_text,
        })
    }
}

fn validate_display_length(
    metadata: DocumentLineMetadata,
    actual: usize,
) -> Result<(), BoundedTextError> {
    let Some(expected) = metadata.display_len.exact() else {
        return Err(BoundedTextError::ExactDisplayLengthUnavailable {
            line_id: metadata.id,
        });
    };
    if actual != expected {
        return Err(BoundedTextError::DisplayLengthMismatch {
            line_id: metadata.id,
            expected,
            actual,
        });
    }
    Ok(())
}
