//! Blocking stdin と CPU-bound search を main thread から隔離する bounded runtime primitive。
//!
//! Reader は受信 byte を [`AppendStore`] へ先に永続化し、appendが返した開始offsetと同じchunkを
//! channelへ渡す。EOFではstoreをsealしてhigh-water markを固定してから[`ReaderEvent::Eof`]を
//! publishする。
//! channel が満杯なら producer 側で backpressure をかけ、未処理 chunk を無制限に保持しない。
//! Search worker は実行中1件とpending 1件だけを持ち、新requestの内部sequenceで旧scanを
//! cooperative cancellationする。Document snapshotはworkerごとのline readerから逐次decodeし、全lineの
//! 参照配列を複製しない。Resultはtaskのquery/document generationをそのまま返し、stateが現在の
//! snapshotと照合する。Panicはfailure eventへ変換する。どちらのthreadもpager終了時にはjoinしない。
//! stdin readや巨大regexの完了待ちで`q`を遅延させず、process終了によって残ったthreadを回収するためである。

use std::io::{self, Read};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use thiserror::Error;

use crate::bounded_text::{BoundedDocumentTextReader, BoundedTextError, BoundedTextRead};
use crate::document::DocumentSnapshot;
use crate::input::{AppendStore, StoreError};
use crate::model::LineId;
use crate::search::{
    ScanOutcome, ScanResult, SearchLine, SearchLineRead, SearchSource, scan_source,
};
use crate::state::{DocumentGeneration, SearchTask};
use crate::text::MAX_SEARCH_LINE_BYTES;

const READER_CHANNEL_CAPACITY: usize = 4;
const READ_CHUNK_BYTES: usize = 32 * 1024;

/// Reader threadからmain loopへ届く入力状態。
#[derive(Debug)]
pub enum ReaderEvent {
    Data {
        /// 同じeventの`bytes`をappendした時に[`AppendStore::append`]が返した絶対offset。
        raw_start: u64,
        /// `raw_start`からstoreへ永続化済みのbyte列。
        bytes: Vec<u8>,
    },
    Eof,
    Failed(ReaderFailure),
}

/// Stdin readまたはraw spoolのfailure。
#[derive(Debug, Error)]
pub enum ReaderFailure {
    #[error("stdin read failed: {0}")]
    Read(#[source] io::Error),
    #[error("stdin spool failed: {0}")]
    Store(#[from] StoreError),
}

/// Reader event streamと、受信済みraw byteのappend-only store。
pub struct ReaderRuntime {
    pub events: Receiver<ReaderEvent>,
    pub store: AppendStore,
}

impl ReaderRuntime {
    /// Process stdinを読むthreadを開始する。
    pub fn spawn_stdin() -> io::Result<Self> {
        Self::spawn(io::stdin())
    }

    /// Testまたは別readerを同じspool/channel契約で開始する。
    pub fn spawn<R>(mut reader: R) -> io::Result<Self>
    where
        R: Read + Send + 'static,
    {
        let store = AppendStore::new();
        let thread_store = store.clone();
        let (sender, events) = bounded(READER_CHANNEL_CAPACITY);
        let join = thread::Builder::new()
            .name("git-ultrapager-stdin".to_owned())
            .spawn(move || reader_loop(&mut reader, &thread_store, &sender))?;
        // JoinHandleのdropはthreadをdetachする。main loopはreceiverをdropするだけでよく、
        // blocked readがEOFになるまで待たない。
        drop(join);
        Ok(Self { events, store })
    }
}

fn reader_loop<R: Read>(reader: &mut R, store: &AppendStore, sender: &Sender<ReaderEvent>) {
    let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => {
                let event = match store.seal() {
                    Ok(_) => ReaderEvent::Eof,
                    Err(error) => ReaderEvent::Failed(ReaderFailure::Store(error)),
                };
                let _ = sender.send(event);
                return;
            }
            Ok(count) => count,
            Err(error) => {
                let _ = sender.send(ReaderEvent::Failed(ReaderFailure::Read(error)));
                return;
            }
        };
        let raw_start = match store.append(&buffer[..count]) {
            Ok(raw_start) => raw_start,
            Err(error) => {
                let _ = sender.send(ReaderEvent::Failed(ReaderFailure::Store(error)));
                return;
            }
        };
        if sender
            .send(ReaderEvent::Data {
                raw_start,
                bytes: buffer[..count].to_vec(),
            })
            .is_err()
        {
            return;
        }
    }
}

/// Search workerが返すquery/document generation付きevent。
#[derive(Clone, Debug)]
pub enum SearchWorkerEvent {
    Completed {
        generation: crate::search::SearchGeneration,
        document_generation: DocumentGeneration,
        result: ScanResult<LineId>,
        preview: bool,
    },
    Failed {
        generation: crate::search::SearchGeneration,
        document_generation: DocumentGeneration,
        preview: bool,
        message: String,
    },
}

/// 1 search requestと、そのrequestが見るimmutable document snapshot。
#[derive(Clone, Debug)]
pub struct SearchJob {
    pub task: SearchTask,
    pub document: Arc<DocumentSnapshot>,
}

#[derive(Clone, Debug)]
struct QueuedSearchJob {
    sequence: u64,
    job: SearchJob,
}

/// 直列bounded search workerのhandle。
pub struct SearchWorker {
    requests: Sender<QueuedSearchJob>,
    queued_requests: Receiver<QueuedSearchJob>,
    latest_sequence: Arc<AtomicU64>,
    submission_gate: Arc<Mutex<()>>,
    next_sequence: AtomicU64,
    pub responses: Receiver<SearchWorkerEvent>,
}

impl SearchWorker {
    /// 実行中1件とpending 1件だけを保持するlatest-wins workerを開始する。
    ///
    /// 2件以上のsnapshotを待機させても最新request以外は適用されず、巨大documentのArcを余分に
    /// 延命するため、pendingとresponseのcapacityはcallerから変更できない。
    pub fn spawn() -> io::Result<Self> {
        let (requests, request_rx) = bounded::<QueuedSearchJob>(1);
        let queued_requests = request_rx.clone();
        let (response_tx, responses) = bounded::<SearchWorkerEvent>(1);
        let response_eviction = responses.clone();
        let latest_sequence = Arc::new(AtomicU64::new(0));
        let worker_latest = Arc::clone(&latest_sequence);
        let submission_gate = Arc::new(Mutex::new(()));
        let worker_gate = Arc::clone(&submission_gate);
        let join = thread::Builder::new()
            .name("git-ultrapager-search".to_owned())
            .spawn(move || {
                search_loop(
                    request_rx,
                    response_tx,
                    response_eviction,
                    &worker_latest,
                    &worker_gate,
                );
            })?;
        drop(join);
        Ok(Self {
            requests,
            queued_requests,
            latest_sequence,
            submission_gate,
            next_sequence: AtomicU64::new(0),
            responses,
        })
    }

    /// Main loopをblockせずrequestをenqueueする。未開始jobと未取得responseを捨て、実行中jobには
    /// sequence変更を通知する。返り値がtrueになった後、旧sequenceのresponseはchannelへ残らない。
    #[must_use]
    pub fn submit_latest(&self, job: SearchJob) -> bool {
        let sequence = self
            .next_sequence
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        // Workerの最終sequence照合と同じgateに置く。Workerが照合直後だった場合は先にpublishさせ、
        // ここでqueueから除去するため、submit完了後に旧responseが観測されるraceを残さない。
        let _gate = lock_unpoisoned(&self.submission_gate);
        self.latest_sequence.store(sequence, Ordering::Release);
        while self.responses.try_recv().is_ok() {}
        let mut queued = QueuedSearchJob { sequence, job };

        loop {
            match self.requests.try_send(queued) {
                Ok(()) => return true,
                Err(TrySendError::Disconnected(_)) => return false,
                Err(TrySendError::Full(returned)) => {
                    queued = returned;
                    match self.queued_requests.try_recv() {
                        Ok(_) | Err(TryRecvError::Empty) => {}
                        Err(TryRecvError::Disconnected) => return false,
                    }
                }
            }
        }
    }
}

struct DocumentSearchSource<'document> {
    document: &'document DocumentSnapshot,
    /// Scanner経由のreadはmetadata capと64 KiB cancellationを共有する。
    bounded_reader: BoundedDocumentTextReader,
}

impl<'document> DocumentSearchSource<'document> {
    fn new(document: &'document DocumentSnapshot) -> Self {
        Self {
            document,
            bounded_reader: BoundedDocumentTextReader::new(),
        }
    }
}

#[derive(Debug, Error)]
enum DocumentSearchSourceError {
    #[error("document search source does not contain logical line {index}")]
    MissingLine { index: usize },
    #[error(transparent)]
    BoundedText(#[from] BoundedTextError),
}

impl SearchSource<LineId> for DocumentSearchSource<'_> {
    type Error = DocumentSearchSourceError;

    fn line_count(&self) -> usize {
        self.document.line_count()
    }

    fn read_line(
        &mut self,
        index: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<SearchLineRead<'_, LineId>, Self::Error> {
        let line_id = self
            .document
            .line_id_at(index)
            .ok_or(DocumentSearchSourceError::MissingLine { index })?;
        match self
            .bounded_reader
            .read(self.document, line_id, MAX_SEARCH_LINE_BYTES, cancelled)?
        {
            BoundedTextRead::Available { metadata, text } => Ok(SearchLineRead::Searchable(
                SearchLine::new(metadata.id, text),
            )),
            BoundedTextRead::Oversized { metadata } => Ok(SearchLineRead::Oversized {
                line_id: metadata.id,
            }),
            BoundedTextRead::Cancelled => Ok(SearchLineRead::Cancelled),
        }
    }
}

fn search_loop(
    requests: Receiver<QueuedSearchJob>,
    responses: Sender<SearchWorkerEvent>,
    response_eviction: Receiver<SearchWorkerEvent>,
    latest_sequence: &AtomicU64,
    submission_gate: &Mutex<()>,
) {
    while let Ok(queued) = requests.recv() {
        let generation = queued.job.task.generation;
        let document_generation = queued.job.task.document_generation;
        let preview = queued.job.task.preview;
        let cancelled = || latest_sequence.load(Ordering::Acquire) != queued.sequence;
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
            let mut source = DocumentSearchSource::new(&queued.job.document);
            scan_source(
                &queued.job.task.compiled,
                &mut source,
                queued.job.task.request,
                &cancelled,
            )
        }));
        let event = match outcome {
            Ok(ScanOutcome::Completed(result)) => SearchWorkerEvent::Completed {
                generation,
                document_generation,
                result,
                preview,
            },
            Ok(ScanOutcome::Cancelled) => continue,
            Ok(ScanOutcome::Failed(error)) => SearchWorkerEvent::Failed {
                generation,
                document_generation,
                preview,
                message: format!("search source failed: {error}"),
            },
            Err(_) => SearchWorkerEvent::Failed {
                generation,
                document_generation,
                preview,
                message: "search worker panicked; current search was discarded".to_owned(),
            },
        };
        let _gate = lock_unpoisoned(submission_gate);
        if latest_sequence.load(Ordering::Acquire) != queued.sequence {
            continue;
        }
        send_latest_response(&responses, &response_eviction, event);
    }
}

fn lock_unpoisoned(mutex: &Mutex<()>) -> MutexGuard<'_, ()> {
    match mutex.lock() {
        Ok(guard) => guard,
        // Gateはsequenceの照合順だけを保護し、poison対象のpayloadを持たない。Scan panicをeventへ
        // 変換した後も排他性だけを再利用する。
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn send_latest_response(
    responses: &Sender<SearchWorkerEvent>,
    response_eviction: &Receiver<SearchWorkerEvent>,
    mut response: SearchWorkerEvent,
) {
    loop {
        match responses.try_send(response) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => return,
            Err(TrySendError::Full(returned)) => {
                response = returned;
                let _ = response_eviction.try_recv();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io::{Cursor, Read};
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use super::*;
    use crate::ansi::AnsiStripper;
    use crate::bounded_text::MAX_EAGER_TEXT_RAW_BYTES;
    use crate::document::{AppendDocumentLine, DocumentBuilder, DocumentError, DocumentSnapshot};
    use crate::document_store::{CHUNKED_LINE_RAW_BYTES, LineStoreError};
    use crate::model::LineKind;
    use crate::search::{
        ScanRequest, SearchAnchor, SearchDirection, SearchGeneration, SearchSession,
        compile_smart_case,
    };
    use crate::storage::StructuralStorage;

    fn document_snapshot_with_display_lengths(
        input: &[u8],
        display_lengths: &[usize],
    ) -> DocumentSnapshot {
        let raw = AppendStore::new();
        raw.append(input).unwrap();
        raw.seal().unwrap();
        let storage = StructuralStorage::new().unwrap();
        let mut builder = DocumentBuilder::new(raw, &storage).unwrap();
        let mut tracker = builder.start_line_tracker().unwrap();
        let mut stripper = AnsiStripper::default();
        let mut spans = Vec::new();
        stripper
            .push_indexed(0, input, |offset, byte| {
                if let Some(span) = tracker.observe(offset, byte).unwrap() {
                    spans.push(span);
                }
            })
            .unwrap();
        stripper.finish_indexed(|offset, byte| {
            if let Some(span) = tracker.observe(offset, byte).unwrap() {
                spans.push(span);
            }
        });
        if let Some(span) = tracker
            .seal_eof(u64::try_from(input.len()).unwrap())
            .unwrap()
        {
            spans.push(span);
        }
        assert_eq!(spans.len(), display_lengths.len());
        for (span, &display_len) in spans.into_iter().zip(display_lengths) {
            builder
                .append_line(
                    span,
                    AppendDocumentLine {
                        display_len,
                        kind: LineKind::Plain,
                        commit_id: None,
                        file_id: None,
                        hunk_id: None,
                        old_line_number: None,
                        new_line_number: None,
                    },
                )
                .unwrap();
        }
        builder.mark_complete();
        builder.snapshot().unwrap()
    }

    fn osc_prefixed_line(payload_bytes: usize, visible: &[u8]) -> Vec<u8> {
        let mut input = Vec::with_capacity(
            b"\x1b]0;".len() + payload_bytes + b"\x07".len() + visible.len() + 1,
        );
        input.extend_from_slice(b"\x1b]0;");
        input.extend(std::iter::repeat_n(b'x', payload_bytes));
        input.push(b'\x07');
        input.extend_from_slice(visible);
        input.push(b'\n');
        input
    }

    fn search_task(generation: SearchGeneration, pattern: &str) -> SearchTask {
        SearchTask {
            generation,
            document_generation: DocumentGeneration::default(),
            compiled: compile_smart_case(pattern).unwrap(),
            request: ScanRequest::new(
                SearchDirection::Forward,
                SearchAnchor::BeforeDocument,
                NonZeroUsize::MIN,
            ),
            preview: false,
        }
    }

    struct ChunkReader {
        chunks: std::vec::IntoIter<Vec<u8>>,
    }

    impl ChunkReader {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: chunks.into_iter(),
            }
        }
    }

    impl Read for ChunkReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let Some(chunk) = self.chunks.next() else {
                return Ok(0);
            };
            assert!(chunk.len() <= buffer.len());
            buffer[..chunk.len()].copy_from_slice(&chunk);
            Ok(chunk.len())
        }
    }

    #[test]
    fn reader_publishes_contiguous_offsets_for_spooled_chunks() {
        let runtime =
            ReaderRuntime::spawn(ChunkReader::new(vec![b"ab".to_vec(), b"cde".to_vec()])).unwrap();
        let first = runtime.events.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            first,
            ReaderEvent::Data {
                raw_start: 0,
                ref bytes,
            } if bytes == b"ab"
        ));
        let second = runtime.events.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            second,
            ReaderEvent::Data {
                raw_start: 2,
                ref bytes,
            } if bytes == b"cde"
        ));
        assert_eq!(runtime.store.read_range(0..5).unwrap(), b"abcde");
    }

    #[test]
    fn reader_seals_store_before_publishing_eof() {
        let runtime = ReaderRuntime::spawn(Cursor::new(b"hello".to_vec())).unwrap();
        assert!(matches!(
            runtime.events.recv_timeout(Duration::from_secs(1)),
            Ok(ReaderEvent::Data { .. })
        ));
        assert!(matches!(
            runtime.events.recv_timeout(Duration::from_secs(1)),
            Ok(ReaderEvent::Eof)
        ));
        assert!(runtime.store.is_sealed().unwrap());
        assert!(matches!(
            runtime.store.append(b"later"),
            Err(StoreError::Sealed { len: 5 })
        ));
    }

    #[test]
    fn document_search_source_decodes_only_the_requested_line_and_preserves_document_errors() {
        // 先頭recordだけdisplay lengthを壊す。2行目の取得が成功すれば、source構築時やscan前に
        // 全lineをdecodeしていないことも同時に固定できる。
        let document = document_snapshot_with_display_lengths(b"alpha\nbeta\n", &[99, 4]);
        let mut source = DocumentSearchSource::new(&document);

        assert_eq!(source.line_count(), 2);
        let second = source.read_line(1, &|| false).unwrap();
        assert!(matches!(
            second,
            SearchLineRead::Searchable(SearchLine {
                id,
                text: "beta"
            }) if id == LineId::new(1)
        ));

        let error = source.read_line(0, &|| false).unwrap_err();
        assert!(matches!(
            error,
            DocumentSearchSourceError::BoundedText(BoundedTextError::Document(
                DocumentError::LineStore(LineStoreError::DisplayLengthMismatch { line_id: 0, .. })
            ))
        ));
    }

    #[test]
    fn metadata_oversized_lines_skip_raw_io_once_in_both_directions() {
        // Raw textと矛盾するdisplay lengthを持たせる。Metadata判定より先にdecodeすると
        // DisplayLengthMismatchになるため、Oversized完了そのものがraw未読の回帰検査になる。
        let document = document_snapshot_with_display_lengths(
            b"x\nneedle\nz\n",
            &[MAX_SEARCH_LINE_BYTES + 1, 6, MAX_SEARCH_LINE_BYTES + 1],
        );
        let compiled = compile_smart_case("needle").unwrap();

        let mut forward_source = DocumentSearchSource::new(&document);
        let forward = scan_source(
            &compiled,
            &mut forward_source,
            ScanRequest::new(
                SearchDirection::Forward,
                SearchAnchor::BeforeDocument,
                NonZeroUsize::MIN,
            ),
            &|| false,
        );
        let ScanOutcome::Completed(forward) = forward else {
            panic!("forward search completed");
        };
        assert_eq!(forward.skipped_oversized_lines, 1);
        assert_eq!(forward.found.unwrap().line_id, LineId::new(1));

        let mut backward_source = DocumentSearchSource::new(&document);
        let backward = scan_source(
            &compiled,
            &mut backward_source,
            ScanRequest::new(
                SearchDirection::Backward,
                SearchAnchor::AfterDocument,
                NonZeroUsize::MIN,
            ),
            &|| false,
        );
        let ScanOutcome::Completed(backward) = backward else {
            panic!("backward search completed");
        };
        assert_eq!(backward.skipped_oversized_lines, 1);
        assert_eq!(backward.found.unwrap().line_id, LineId::new(1));
    }

    #[test]
    fn raw_huge_small_display_line_remains_searchable_from_both_anchor_directions() {
        let visible = b"needle middle needle";
        let input = osc_prefixed_line(MAX_SEARCH_LINE_BYTES + 1, visible);
        let document = document_snapshot_with_display_lengths(&input, &[visible.len()]);
        let compiled = compile_smart_case("needle").unwrap();
        let cases = [
            (SearchDirection::Forward, SearchAnchor::BeforeDocument, 0..6),
            (SearchDirection::Forward, SearchAnchor::at(0, 1), 14..20),
            (
                SearchDirection::Backward,
                SearchAnchor::AfterDocument,
                14..20,
            ),
            (SearchDirection::Backward, SearchAnchor::at(0, 19), 0..6),
        ];

        for (direction, anchor, expected_range) in cases {
            let mut source = DocumentSearchSource::new(&document);
            let outcome = scan_source(
                &compiled,
                &mut source,
                ScanRequest::new(direction, anchor, NonZeroUsize::MIN),
                &|| false,
            );
            let ScanOutcome::Completed(result) = outcome else {
                panic!("bounded search completed");
            };
            assert_eq!(result.skipped_oversized_lines, 0);
            assert_eq!(result.found.unwrap().byte_range, expected_range);
            assert!(
                source.bounded_reader.large_line_stats().raw_bytes_read
                    > u64::try_from(MAX_EAGER_TEXT_RAW_BYTES).unwrap()
            );
        }
    }

    #[test]
    fn ordinary_multi_megabyte_line_can_cancel_after_one_streaming_chunk() {
        let visible_len = MAX_EAGER_TEXT_RAW_BYTES * 2;
        let mut input = vec![b'a'; visible_len];
        input.push(b'\n');
        let document = document_snapshot_with_display_lengths(&input, &[visible_len]);
        let mut source = DocumentSearchSource::new(&document);
        let checks = Cell::new(0_usize);
        let read = source
            .read_line(0, &|| {
                let current = checks.get().saturating_add(1);
                checks.set(current);
                current >= 3
            })
            .unwrap();

        assert!(matches!(read, SearchLineRead::Cancelled));
        assert_eq!(
            source.bounded_reader.large_line_stats().raw_bytes_read,
            u64::try_from(CHUNKED_LINE_RAW_BYTES).unwrap()
        );
    }

    #[test]
    fn ansi_only_payload_checks_cancellation_on_progress_without_visible_units() {
        let input = osc_prefixed_line(MAX_EAGER_TEXT_RAW_BYTES * 2, b"");
        let document = document_snapshot_with_display_lengths(&input, &[0]);
        let mut source = DocumentSearchSource::new(&document);
        let checks = Cell::new(0_usize);
        let read = source
            .read_line(0, &|| {
                let current = checks.get().saturating_add(1);
                checks.set(current);
                current >= 4
            })
            .unwrap();

        assert!(matches!(read, SearchLineRead::Cancelled));
        assert_eq!(
            source.bounded_reader.large_line_stats().raw_bytes_read,
            u64::try_from(CHUNKED_LINE_RAW_BYTES * 2).unwrap()
        );
    }

    #[test]
    fn search_worker_returns_owned_result() {
        let worker = SearchWorker::spawn().unwrap();
        let document = Arc::new(document_snapshot_with_display_lengths(
            b"alpha\nbeta\n",
            &[5, 4],
        ));
        let task = search_task(SearchGeneration::default(), "beta");
        let expected_document_generation = task.document_generation;
        assert!(worker.submit_latest(SearchJob { task, document }));
        let result = worker
            .responses
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(matches!(
            result,
            SearchWorkerEvent::Completed {
                document_generation,
                result: ScanResult { found: Some(_), .. },
                ..
            } if document_generation == expected_document_generation
        ));
    }

    #[test]
    fn search_worker_keeps_one_pending_job_and_one_response() {
        let worker = SearchWorker::spawn().unwrap();

        assert_eq!(worker.requests.capacity(), Some(1));
        assert_eq!(worker.responses.capacity(), Some(1));
    }

    #[test]
    fn submitting_a_replacement_removes_or_suppresses_stale_responses() {
        let worker = SearchWorker::spawn().unwrap();
        let stale_input = osc_prefixed_line(MAX_EAGER_TEXT_RAW_BYTES * 2, b"alpha");
        let stale_document = Arc::new(document_snapshot_with_display_lengths(&stale_input, &[5]));
        let current_document = Arc::new(document_snapshot_with_display_lengths(b"beta\n", &[4]));
        let mut session = SearchSession::<LineId>::default();
        let first_generation = session.begin_navigation_intent();
        let second_generation = session.begin_navigation_intent();

        assert!(worker.submit_latest(SearchJob {
            task: search_task(first_generation, "missing"),
            document: stale_document,
        }));
        // Workerが64 KiB streaming decodeへ入る機会を与える。先のresultをすでにpublishした場合も、
        // 次のsubmitが同じgate内でresponseを空にするため、submit完了後に旧世代は残らない。
        std::thread::sleep(Duration::from_millis(1));
        assert!(worker.submit_latest(SearchJob {
            task: search_task(second_generation, "beta"),
            document: current_document,
        }));

        let response = worker
            .responses
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(matches!(
            response,
            SearchWorkerEvent::Completed {
                generation,
                result: ScanResult { found: Some(_), .. },
                ..
            } if generation == second_generation
        ));
        assert!(
            worker
                .responses
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );
    }
}
