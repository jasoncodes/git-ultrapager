//! Blocking stdin と CPU-bound search を main thread から隔離する bounded runtime primitive。
//!
//! Reader は受信 byte を [`AppendStore`] へ先に永続化してから同じ chunk を channel へ渡す。EOFでは
//! storeをsealしてhigh-water markを固定してから[`ReaderEvent::Eof`]をpublishする。
//! channel が満杯なら producer 側で backpressure をかけ、未処理 chunk を無制限に保持しない。
//! Search worker は実行中1件とpending 1件だけを持ち、新requestの内部sequenceで旧scanを
//! cooperative cancellationする。Documentはline accessorから逐次読み、全lineの参照配列を複製しない。
//! Panicはfailure eventへ変換する。どちらのthreadもpager終了時にはjoinしない。stdin readや巨大regex
//! の完了待ちで`q`を遅延させず、process終了によって残ったthreadを回収するためである。

use std::io::{self, Read};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use thiserror::Error;

use crate::input::{AppendStore, StoreError};
use crate::model::{Document, LineId};
use crate::search::{ScanOutcome, ScanResult, SearchLine, SearchSource, scan_source};
use crate::state::SearchTask;

const READER_CHANNEL_CAPACITY: usize = 4;
const READ_CHUNK_BYTES: usize = 32 * 1024;

/// Reader threadからmain loopへ届く入力状態。
#[derive(Debug)]
pub enum ReaderEvent {
    Data(Vec<u8>),
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
        if let Err(error) = store.append(&buffer[..count]) {
            let _ = sender.send(ReaderEvent::Failed(ReaderFailure::Store(error)));
            return;
        }
        if sender
            .send(ReaderEvent::Data(buffer[..count].to_vec()))
            .is_err()
        {
            return;
        }
    }
}

/// Search workerが返すgeneration付きevent。
#[derive(Clone, Debug)]
pub enum SearchWorkerEvent {
    Completed {
        generation: crate::search::SearchGeneration,
        result: ScanResult<LineId>,
        preview: bool,
    },
    Failed {
        generation: crate::search::SearchGeneration,
        message: String,
    },
}

/// 1 search requestと、そのrequestが見るimmutable document snapshot。
#[derive(Clone, Debug)]
pub struct SearchJob {
    pub task: SearchTask,
    pub document: Arc<Document>,
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
    document: &'document Document,
}

#[derive(Clone, Copy, Debug, Error)]
#[error("document search source does not contain logical line {index}")]
struct DocumentSearchSourceError {
    index: usize,
}

impl SearchSource<LineId> for DocumentSearchSource<'_> {
    type Error = DocumentSearchSourceError;

    fn line_count(&self) -> usize {
        self.document.lines().len()
    }

    fn line(&mut self, index: usize) -> Result<SearchLine<'_, LineId>, Self::Error> {
        self.document
            .lines()
            .get(index)
            .map(|line| SearchLine::new(line.id, line.text.as_str()))
            .ok_or(DocumentSearchSourceError { index })
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
        let preview = queued.job.task.preview;
        let cancelled = || latest_sequence.load(Ordering::Acquire) != queued.sequence;
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
            let mut source = DocumentSearchSource {
                document: &queued.job.document,
            };
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
                result,
                preview,
            },
            Ok(ScanOutcome::Cancelled) => continue,
            Ok(ScanOutcome::Failed(error)) => SearchWorkerEvent::Failed {
                generation,
                message: format!("search source failed: {error}"),
            },
            Err(_) => SearchWorkerEvent::Failed {
                generation,
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
    use std::io::Cursor;
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use super::*;
    use crate::parser::parse_bytes;
    use crate::search::{
        ScanRequest, SearchAnchor, SearchDirection, SearchGeneration, SearchSession,
        compile_smart_case,
    };

    fn search_task(generation: SearchGeneration, pattern: &str) -> SearchTask {
        SearchTask {
            generation,
            compiled: compile_smart_case(pattern).unwrap(),
            request: ScanRequest::new(
                SearchDirection::Forward,
                SearchAnchor::BeforeDocument,
                NonZeroUsize::MIN,
            ),
            preview: false,
        }
    }

    #[test]
    fn reader_spools_before_publishing_chunks() {
        let runtime = ReaderRuntime::spawn(Cursor::new(b"hello".to_vec())).unwrap();
        let first = runtime.events.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(first, ReaderEvent::Data(ref bytes) if bytes == b"hello"));
        assert_eq!(runtime.store.read_range(0..5).unwrap(), b"hello");
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
    fn search_worker_returns_owned_result() {
        let worker = SearchWorker::spawn().unwrap();
        let document = Arc::new(parse_bytes(b"alpha\nbeta\n"));
        let task = search_task(SearchGeneration::default(), "beta");
        assert!(worker.submit_latest(SearchJob { task, document }));
        let result = worker
            .responses
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(matches!(
            result,
            SearchWorkerEvent::Completed {
                result: ScanResult { found: Some(_), .. },
                ..
            }
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
        let document = Arc::new(parse_bytes(b"alpha\nbeta\n"));
        let mut session = SearchSession::<LineId>::default();
        let first_generation = session.begin_navigation_intent();
        let second_generation = session.begin_navigation_intent();

        assert!(worker.submit_latest(SearchJob {
            task: search_task(first_generation, "alpha"),
            document: Arc::clone(&document),
        }));
        // Workerが先のresultをすでにpublishした場合も、次のsubmitが同じgate内でqueueを空にする。
        std::thread::sleep(Duration::from_millis(10));
        assert!(worker.submit_latest(SearchJob {
            task: search_task(second_generation, "beta"),
            document,
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
