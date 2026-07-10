//! Blocking stdin と CPU-bound search を main thread から隔離する bounded runtime primitive。
//!
//! Reader は受信 byte を [`AppendStore`] へ先に永続化してから同じ chunk を channel へ渡す。
//! channel が満杯なら producer 側で backpressure をかけ、未処理 chunk を無制限に保持しない。
//! Search worker は owned document snapshot と generation付きtaskを直列実行し、panicをfailure eventへ
//! 変換する。どちらのthreadもpager終了時にはjoinしない。stdin readや巨大regexの完了待ちで`q`を
//! 遅延させず、process終了によって残ったthreadを回収するためである。

use std::io::{self, Read};
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use thiserror::Error;

use crate::input::{AppendStore, StoreError};
use crate::model::{Document, LineId};
use crate::search::{ScanResult, SearchLine, scan_lines};
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
                let _ = sender.send(ReaderEvent::Eof);
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

/// 直列bounded search workerのhandle。
pub struct SearchWorker {
    requests: Sender<SearchJob>,
    queued_requests: Receiver<SearchJob>,
    pub responses: Receiver<SearchWorkerEvent>,
}

impl SearchWorker {
    /// Queue capacityを最低1へ丸めてworker threadを開始する。
    pub fn spawn(queue_capacity: usize) -> io::Result<Self> {
        let capacity = queue_capacity.max(1);
        let (requests, request_rx) = bounded::<SearchJob>(capacity);
        let queued_requests = request_rx.clone();
        let (response_tx, responses) = bounded::<SearchWorkerEvent>(capacity);
        let join = thread::Builder::new()
            .name("git-ultrapager-search".to_owned())
            .spawn(move || search_loop(request_rx, response_tx))?;
        drop(join);
        Ok(Self {
            requests,
            queued_requests,
            responses,
        })
    }

    /// Main loopをblockせずrequestをenqueueする。Queueが満杯なら未開始の旧jobを捨てて最新jobへ
    /// 置き換える。実行中jobは止めず、generation不一致としてstate側で破棄する。
    #[must_use]
    pub fn submit_latest(&self, mut job: SearchJob) -> bool {
        loop {
            match self.requests.try_send(job) {
                Ok(()) => return true,
                Err(TrySendError::Disconnected(_)) => return false,
                Err(TrySendError::Full(returned)) => {
                    job = returned;
                    match self.queued_requests.try_recv() {
                        Ok(_) | Err(crossbeam_channel::TryRecvError::Empty) => {}
                        Err(crossbeam_channel::TryRecvError::Disconnected) => return false,
                    }
                }
            }
        }
    }
}

fn search_loop(requests: Receiver<SearchJob>, responses: Sender<SearchWorkerEvent>) {
    while let Ok(job) = requests.recv() {
        let generation = job.task.generation;
        let preview = job.task.preview;
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
            let lines = job
                .document
                .lines()
                .iter()
                .map(|line| SearchLine::new(line.id, line.text.as_str()))
                .collect::<Vec<_>>();
            scan_lines(&job.task.compiled, &lines, job.task.request)
        }));
        let event = match outcome {
            Ok(result) => SearchWorkerEvent::Completed {
                generation,
                result,
                preview,
            },
            Err(_) => SearchWorkerEvent::Failed {
                generation,
                message: "search worker panicked; current search was discarded".to_owned(),
            },
        };
        if responses.send(event).is_err() {
            return;
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
    use crate::search::{ScanRequest, SearchAnchor, SearchDirection, compile_smart_case};

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
    }

    #[test]
    fn search_worker_returns_owned_result() {
        let worker = SearchWorker::spawn(1).unwrap();
        let document = Arc::new(parse_bytes(b"alpha\nbeta\n"));
        let task = SearchTask {
            generation: crate::search::SearchGeneration::default(),
            compiled: compile_smart_case("beta").unwrap(),
            request: ScanRequest::new(
                SearchDirection::Forward,
                SearchAnchor::BeforeDocument,
                NonZeroUsize::MIN,
            ),
            preview: false,
        };
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
}
