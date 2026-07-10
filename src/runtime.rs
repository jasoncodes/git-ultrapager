//! Blocking stdin、CPU-bound search、file path filter を main thread から隔離する bounded runtime primitive。
//!
//! Reader は受信 byte を [`AppendStore`] へ先に永続化し、appendが返した開始offsetと同じchunkを
//! channelへ渡す。EOFではstoreをsealしてhigh-water markを固定してから[`ReaderEvent::Eof`]を
//! publishする。
//! channel が満杯なら producer 側で backpressure をかけ、未処理 chunk を無制限に保持しない。
//! Search / file-filter worker は実行中1件とpending 1件だけを持ち、新requestまたは明示cancelの
//! 内部sequenceで旧scanをcooperative cancellationする。Document snapshotはworkerごとのline readerから逐次decodeし、全lineの
//! 参照配列を複製しない。File filterはcommit metadataとpath readerを再利用し、Git順の全matchを
//! materializeせずselection周辺5,000件だけを返す。Resultはtaskのquery/document generationをそのまま
//! 返し、stateが現在のsnapshotと照合する。Panicはfailure eventへ変換する。各threadはpager終了時に
//! joinしない。
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
use crate::model::{FileId, LineId};
use crate::search::{
    ScanOutcome, ScanResult, SearchLine, SearchLineRead, SearchSource, scan_source,
};
use crate::state::{
    DocumentGeneration, FILE_FILTER_WINDOW_CAPACITY, FileFilterGeneration, FileFilterTask,
    MAX_FILE_FILTER_QUERY_BYTES, SearchTask,
};
use crate::structure_store::{
    StoredFileRef, StructureMetadataReader, StructureReader, StructureStoreError,
};
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
        // Workerの最終sequence照合と同じgateに置く。Workerが照合直後だった場合は先にpublishさせ、
        // ここでqueueから除去するため、submit完了後に旧responseが観測されるraceを残さない。
        let _gate = lock_unpoisoned(&self.submission_gate);
        let sequence = self
            .next_sequence
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
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

    /// Main loopをblockせず、実行中と未開始のsearchを取り消す。
    ///
    /// Sequence更新、pending jobの除去、未取得responseの除去をworkerのpublish照合と
    /// 同じgate内で実行する。この関数が返った後に取消対象のresponseはchannelへ残らず、
    /// 実行中jobは次のcancellation照合でsnapshotを解放する。
    pub fn cancel_latest(&self) {
        cancel_latest_work(
            &self.next_sequence,
            &self.latest_sequence,
            &self.submission_gate,
            &self.queued_requests,
            &self.responses,
        );
    }
}

/// File filter workerが返す世代付きwindow。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileFilterResult {
    /// Query editまたはwindow requestを識別するtask token。
    pub generation: FileFilterGeneration,
    /// Workerが走査したimmutable snapshotのstate世代。
    pub document_generation: DocumentGeneration,
    /// Queryに一致したfile occurrence総数。
    pub total_matches: u64,
    /// 全match内のselection rank。Matchなしでは`None`。
    pub selected_rank: Option<u64>,
    /// `file_ids[0]`が表す全match内のrank。
    pub start_rank: u64,
    /// Git出力順のstable ID。長さは[`FILE_FILTER_WINDOW_CAPACITY`]以下。
    pub file_ids: Vec<FileId>,
}

/// File filter workerからmain loopへ届くevent。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileFilterWorkerEvent {
    /// 最新sequenceの2-pass scanが完了した。
    Completed(FileFilterResult),
    /// 最新sequenceのstructure read、allocation、算術、またはworker panicが失敗した。
    Failed {
        generation: FileFilterGeneration,
        document_generation: DocumentGeneration,
        message: String,
    },
}

/// 1 file-filter requestと、そのrequestが見るimmutable document snapshot。
#[derive(Clone, Debug)]
pub struct FileFilterJob {
    /// Selectionとqueryを固定したowned request。
    pub task: FileFilterTask,
    /// Scan完了またはcancelまで構造storeを生存させるsnapshot。
    pub document: Arc<DocumentSnapshot>,
}

#[derive(Clone, Debug)]
struct QueuedFileFilterJob {
    sequence: u64,
    job: FileFilterJob,
}

/// 実行中1件とpending 1件だけを持つfile-filter workerのhandle。
pub struct FileFilterWorker {
    requests: Sender<QueuedFileFilterJob>,
    queued_requests: Receiver<QueuedFileFilterJob>,
    latest_sequence: Arc<AtomicU64>,
    submission_gate: Arc<Mutex<()>>,
    next_sequence: AtomicU64,
    /// Main loopがnon-blockingにpollするcapacity 1のresponse channel。
    pub responses: Receiver<FileFilterWorkerEvent>,
}

impl FileFilterWorker {
    /// File pathをbackgroundで走査するlatest-wins workerを開始する。
    ///
    /// Pending requestと未取得responseはいずれも1件に制限する。各jobはsnapshotを所有するため、
    /// 古いrequestを複数待機させるとstateから外れた巨大documentを延命してしまう。
    pub fn spawn() -> io::Result<Self> {
        let (requests, request_rx) = bounded::<QueuedFileFilterJob>(1);
        let queued_requests = request_rx.clone();
        let (response_tx, responses) = bounded::<FileFilterWorkerEvent>(1);
        let response_eviction = responses.clone();
        let latest_sequence = Arc::new(AtomicU64::new(0));
        let worker_latest = Arc::clone(&latest_sequence);
        let submission_gate = Arc::new(Mutex::new(()));
        let worker_gate = Arc::clone(&submission_gate);
        let join = thread::Builder::new()
            .name("git-ultrapager-file-filter".to_owned())
            .spawn(move || {
                file_filter_loop(
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

    /// Main loopをblockせずrequestを置き換える。
    ///
    /// `true`を返した後は旧sequenceのresponseをchannelから除去済みである。実行中scanはfile decode前と
    /// path照合中の1,024 scalarごとにsequence変更を検出する。1 fileのdecodeはparserが各1 MiBに制限した
    /// old/new path blobを[`StructureReader`]が一括復元するため、その最大2 readの途中ではcancelしない。
    #[must_use]
    pub fn submit_latest(&self, job: FileFilterJob) -> bool {
        let _gate = lock_unpoisoned(&self.submission_gate);
        let sequence = self
            .next_sequence
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        self.latest_sequence.store(sequence, Ordering::Release);
        while self.responses.try_recv().is_ok() {}
        let mut queued = QueuedFileFilterJob { sequence, job };

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

    /// Main loopをblockせず、実行中と未開始のfile filterを取り消す。
    ///
    /// Sequence更新とpending job / responseの除去をpublish gate内でひとまとめに行う。
    /// 実行中scanはpath decode間のcooperative cancellationで停止し、保持中の
    /// [`DocumentSnapshot`]を代替jobなしで解放する。
    pub fn cancel_latest(&self) {
        cancel_latest_work(
            &self.next_sequence,
            &self.latest_sequence,
            &self.submission_gate,
            &self.queued_requests,
            &self.responses,
        );
    }
}

fn cancel_latest_work<Request, Response>(
    next_sequence: &AtomicU64,
    latest_sequence: &AtomicU64,
    submission_gate: &Mutex<()>,
    queued_requests: &Receiver<Request>,
    responses: &Receiver<Response>,
) {
    let _gate = lock_unpoisoned(submission_gate);
    let sequence = next_sequence
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    latest_sequence.store(sequence, Ordering::Release);
    while queued_requests.try_recv().is_ok() {}
    while responses.try_recv().is_ok() {}
}

#[derive(Debug, Error)]
enum FileFilterError {
    #[error(transparent)]
    Structure(#[from] StructureStoreError),
    #[error("file filter could not reserve {requested} {resource} entries")]
    Allocation {
        resource: &'static str,
        requested: usize,
    },
    #[error("file filter arithmetic overflow while computing {operation}")]
    ArithmeticOverflow { operation: &'static str },
    #[error("file filter query is {actual} bytes, exceeding the {limit}-byte limit")]
    QueryTooLong { actual: usize, limit: usize },
    #[error("structure snapshot does not contain commit {commit_id}")]
    MissingCommit { commit_id: u64 },
    #[error("structure snapshot does not contain file {file_id} owned by commit {commit_id}")]
    MissingFile { file_id: u64, commit_id: u64 },
    #[error(
        "commit {commit_id} starts its file range at {actual}, but the Git-order cursor is {expected}"
    )]
    NonContiguousFiles {
        commit_id: u64,
        expected: u64,
        actual: u64,
    },
    #[error("commit ranges expose {visited} files, but the snapshot contains {visible}")]
    UncoveredFiles { visited: u64, visible: u64 },
    #[error("file filter collected {actual} IDs, but its selected window requires {expected}")]
    WindowLengthMismatch { expected: usize, actual: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileFilterScanOutcome {
    Completed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileVisitDirective {
    Continue,
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathMatchOutcome {
    Matched,
    NotMatched,
    Cancelled,
}

const PATH_CANCELLATION_INTERVAL_CHARS: usize = 1_024;
const DISPLAY_PATH_SEPARATOR: &str = " → ";

/// File filter threadがjobと2 passの間で再利用するdecode buffer。
///
/// Snapshotへのborrowは各readで終わり、保持するのはfixed recordと直近pathのbyte bufferだけである。
/// Workerが古いdocumentのArcを解放した後に、そのsnapshotへの参照は残さない。
#[derive(Debug, Default)]
struct FileFilterReaders {
    metadata: StructureMetadataReader,
    paths: StructureReader,
}

impl FileFilterReaders {
    const fn new() -> Self {
        Self {
            metadata: StructureMetadataReader::new(),
            paths: StructureReader::new(),
        }
    }
}

fn file_filter_loop(
    requests: Receiver<QueuedFileFilterJob>,
    responses: Sender<FileFilterWorkerEvent>,
    response_eviction: Receiver<FileFilterWorkerEvent>,
    latest_sequence: &AtomicU64,
    submission_gate: &Mutex<()>,
) {
    let mut readers = FileFilterReaders::new();
    while let Ok(queued) = requests.recv() {
        let generation = queued.job.task.generation;
        let document_generation = queued.job.task.document_generation;
        let cancelled = || latest_sequence.load(Ordering::Acquire) != queued.sequence;
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
            filter_file_paths(
                &queued.job.task,
                &queued.job.document,
                &mut readers,
                &cancelled,
            )
        }));
        let event = match outcome {
            Ok(Ok(Some(result))) => FileFilterWorkerEvent::Completed(result),
            Ok(Ok(None)) => continue,
            Ok(Err(error)) => FileFilterWorkerEvent::Failed {
                generation,
                document_generation,
                message: format!("file filter failed: {error}"),
            },
            Err(_) => FileFilterWorkerEvent::Failed {
                generation,
                document_generation,
                message: "file filter worker panicked; current filter result was discarded"
                    .to_owned(),
            },
        };
        let _gate = lock_unpoisoned(submission_gate);
        if latest_sequence.load(Ordering::Acquire) != queued.sequence {
            continue;
        }
        send_latest_file_filter_response(&responses, &response_eviction, event);
    }
}

/// Query全体をlowercase scalar列へ1回だけ変換する。Candidate pathは変換先stringを作らず走査する。
fn lowercase_query(
    query: &str,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<Vec<char>>, FileFilterError> {
    if query.len() > MAX_FILE_FILTER_QUERY_BYTES {
        return Err(FileFilterError::QueryTooLong {
            actual: query.len(),
            limit: MAX_FILE_FILTER_QUERY_BYTES,
        });
    }
    // Lowercase expansion後の長さを先に数える。ASCII queryにiteratorの最大展開数を掛けた余剰capacityを
    // 残さず、2回目の走査では確保済み領域へだけ書く。
    let mut capacity = 0_usize;
    let mut chars_since_cancel = 0_usize;
    for scalar in query.chars() {
        chars_since_cancel += 1;
        if chars_since_cancel >= PATH_CANCELLATION_INTERVAL_CHARS {
            chars_since_cancel = 0;
            if cancelled() {
                return Ok(None);
            }
        }
        capacity = capacity.checked_add(scalar.to_lowercase().count()).ok_or(
            FileFilterError::ArithmeticOverflow {
                operation: "lowercase query capacity",
            },
        )?;
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(capacity)
        .map_err(|_| FileFilterError::Allocation {
            resource: "query scalar",
            requested: capacity,
        })?;
    chars_since_cancel = 0;
    for scalar in query.chars() {
        chars_since_cancel += 1;
        if chars_since_cancel >= PATH_CANCELLATION_INTERVAL_CHARS {
            chars_since_cancel = 0;
            if cancelled() {
                return Ok(None);
            }
        }
        output.extend(scalar.to_lowercase());
    }
    Ok(Some(output))
}

/// 全match件数とselection rankを1 passで求め、2 pass目でbounded windowだけを所有する。
fn filter_file_paths(
    task: &FileFilterTask,
    document: &DocumentSnapshot,
    readers: &mut FileFilterReaders,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<FileFilterResult>, FileFilterError> {
    let Some(query) = lowercase_query(&task.query, cancelled)? else {
        return Ok(None);
    };
    let mut total_matches = 0_u64;
    let mut preferred_rank = None;
    let first_pass = visit_matching_files(document, readers, &query, cancelled, |file_id| {
        let rank = total_matches;
        total_matches =
            total_matches
                .checked_add(1)
                .ok_or(FileFilterError::ArithmeticOverflow {
                    operation: "total file matches",
                })?;
        if task.preferred_file == Some(file_id) {
            preferred_rank = Some(rank);
        }
        Ok(FileVisitDirective::Continue)
    })?;
    if first_pass == FileFilterScanOutcome::Cancelled || cancelled() {
        return Ok(None);
    }

    let selected_rank = if total_matches == 0 {
        None
    } else {
        preferred_rank
            .or_else(|| task.requested_rank.map(|rank| rank.min(total_matches - 1)))
            .or(Some(0))
    };
    let capacity = u64::try_from(FILE_FILTER_WINDOW_CAPACITY).map_err(|_| {
        FileFilterError::ArithmeticOverflow {
            operation: "file-filter window capacity",
        }
    })?;
    let start_rank = selected_rank.map_or(0, |selected| {
        selected
            .saturating_sub(capacity / 2)
            .min(total_matches.saturating_sub(capacity))
    });
    let end_rank = start_rank
        .checked_add(capacity)
        .ok_or(FileFilterError::ArithmeticOverflow {
            operation: "file-filter window end",
        })?
        .min(total_matches);
    let window_len_u64 =
        end_rank
            .checked_sub(start_rank)
            .ok_or(FileFilterError::ArithmeticOverflow {
                operation: "file-filter window length",
            })?;
    let window_len =
        usize::try_from(window_len_u64).map_err(|_| FileFilterError::ArithmeticOverflow {
            operation: "file-filter window length conversion",
        })?;
    let mut file_ids = Vec::new();
    file_ids
        .try_reserve_exact(window_len)
        .map_err(|_| FileFilterError::Allocation {
            resource: "file ID",
            requested: window_len,
        })?;

    if window_len > 0 {
        let mut rank = 0_u64;
        let second_pass = visit_matching_files(document, readers, &query, cancelled, |file_id| {
            let current = rank;
            rank = rank
                .checked_add(1)
                .ok_or(FileFilterError::ArithmeticOverflow {
                    operation: "second-pass match rank",
                })?;
            if current >= start_rank && current < end_rank {
                file_ids.push(file_id);
            }
            Ok(if rank >= end_rank {
                FileVisitDirective::Stop
            } else {
                FileVisitDirective::Continue
            })
        })?;
        if second_pass == FileFilterScanOutcome::Cancelled || cancelled() {
            return Ok(None);
        }
    }
    if file_ids.len() != window_len {
        return Err(FileFilterError::WindowLengthMismatch {
            expected: window_len,
            actual: file_ids.len(),
        });
    }
    debug_assert!(file_ids.len() <= FILE_FILTER_WINDOW_CAPACITY);
    Ok(Some(FileFilterResult {
        generation: task.generation,
        document_generation: task.document_generation,
        total_matches,
        selected_rank,
        start_rank,
        file_ids,
    }))
}

/// Commit child rangeをGit順に辿り、queryに一致するfileだけをcallbackへ渡す。
///
/// Commitは固定長metadata readerで各pass 1回だけdecodeする。Path readerにはその親metadataを渡すため、
/// fileごとのcommit再decodeとblob stringの全件materializeを行わない。
fn visit_matching_files<F>(
    document: &DocumentSnapshot,
    readers: &mut FileFilterReaders,
    query: &[char],
    cancelled: &dyn Fn() -> bool,
    mut on_match: F,
) -> Result<FileFilterScanOutcome, FileFilterError>
where
    F: FnMut(FileId) -> Result<FileVisitDirective, FileFilterError>,
{
    let snapshot = document.structure();
    let counts = snapshot.visible_counts()?;
    let FileFilterReaders {
        metadata: metadata_reader,
        paths: path_reader,
    } = readers;
    let mut file_cursor = 0_u64;

    for ordinal in 0..counts.commits {
        if cancelled() {
            return Ok(FileFilterScanOutcome::Cancelled);
        }
        let commit_id = snapshot
            .commit_id_at(ordinal)
            .ok_or(FileFilterError::MissingCommit { commit_id: ordinal })?;
        let parent = metadata_reader.read_commit(snapshot, commit_id)?.ok_or(
            FileFilterError::MissingCommit {
                commit_id: commit_id.get(),
            },
        )?;
        if parent.files.start() != file_cursor {
            return Err(FileFilterError::NonContiguousFiles {
                commit_id: commit_id.get(),
                expected: file_cursor,
                actual: parent.files.start(),
            });
        }
        for file_id in parent.files.iter() {
            if cancelled() {
                return Ok(FileFilterScanOutcome::Cancelled);
            }
            // StructureReaderはold/new blobを1 API callで復元する。Production parserは各pathの元lineを
            // MAX_SYNTAX_LINE_BYTESでfallbackへ落とすため、このcancel不可区間は最大2 MiB相当のreadで
            // 閉じる。Decode後のscalar走査はdisplayed_path_matchesが1,024文字ごとにcancelする。
            let file = path_reader
                .read_file_in_commit(snapshot, file_id, parent)?
                .ok_or(FileFilterError::MissingFile {
                    file_id: file_id.get(),
                    commit_id: commit_id.get(),
                })?;
            file_cursor =
                file_cursor
                    .checked_add(1)
                    .ok_or(FileFilterError::ArithmeticOverflow {
                        operation: "Git-order file cursor",
                    })?;
            match displayed_path_matches(query, file, cancelled) {
                PathMatchOutcome::Matched => {
                    if on_match(file_id)? == FileVisitDirective::Stop {
                        return Ok(FileFilterScanOutcome::Completed);
                    }
                }
                PathMatchOutcome::NotMatched => {}
                PathMatchOutcome::Cancelled => {
                    return Ok(FileFilterScanOutcome::Cancelled);
                }
            }
        }
    }
    if file_cursor != counts.files {
        return Err(FileFilterError::UncoveredFiles {
            visited: file_cursor,
            visible: counts.files,
        });
    }
    Ok(FileFilterScanOutcome::Completed)
}

fn displayed_path_matches(
    query: &[char],
    file: StoredFileRef<'_>,
    cancelled: &dyn Fn() -> bool,
) -> PathMatchOutcome {
    if query.is_empty() {
        return PathMatchOutcome::Matched;
    }
    let mut matched = 0_usize;
    let mut chars_since_cancel = 0_usize;
    let mut consume = |piece: &str| {
        for scalar in piece.chars() {
            chars_since_cancel += 1;
            if chars_since_cancel >= PATH_CANCELLATION_INTERVAL_CHARS {
                chars_since_cancel = 0;
                if cancelled() {
                    return PathMatchOutcome::Cancelled;
                }
            }
            for lowercase in scalar.to_lowercase() {
                if query.get(matched) == Some(&lowercase) {
                    matched += 1;
                    if matched == query.len() {
                        return PathMatchOutcome::Matched;
                    }
                }
            }
        }
        PathMatchOutcome::NotMatched
    };

    match (file.old_path, file.new_path) {
        (Some(old), Some(new)) if old != new => {
            for piece in [old, DISPLAY_PATH_SEPARATOR, new] {
                match consume(piece) {
                    PathMatchOutcome::NotMatched => {}
                    outcome => return outcome,
                }
            }
        }
        (Some(path), _) | (_, Some(path)) => return consume(path),
        (None, None) => return consume("(unparsed path)"),
    }
    PathMatchOutcome::NotMatched
}

fn send_latest_file_filter_response(
    responses: &Sender<FileFilterWorkerEvent>,
    response_eviction: &Receiver<FileFilterWorkerEvent>,
    mut response: FileFilterWorkerEvent,
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
                message: "search worker panicked; current search result was discarded".to_owned(),
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
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::ansi::AnsiStripper;
    use crate::bounded_text::MAX_EAGER_TEXT_RAW_BYTES;
    use crate::document::{AppendDocumentLine, DocumentBuilder, DocumentError, DocumentSnapshot};
    use crate::document_store::{CHUNKED_LINE_RAW_BYTES, LineStoreError};
    use crate::model::LineKind;
    use crate::parser::GitParser;
    use crate::search::{
        ScanRequest, SearchAnchor, SearchDirection, SearchGeneration, SearchSession,
        compile_smart_case,
    };
    use crate::storage::{StorageConfig, StructuralStorage};

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
        builder.mark_complete().unwrap();
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

    fn parse_with_storage(input: &[u8], storage: &StructuralStorage) -> DocumentSnapshot {
        let raw = AppendStore::new();
        let mut parser = GitParser::new(raw.clone(), storage).unwrap();
        let raw_start = raw.append(input).unwrap();
        parser.push(raw_start, input).unwrap();
        raw.seal().unwrap();
        parser.into_document().unwrap()
    }

    fn append_commit(input: &mut String, ordinal: u64) {
        input.push_str(&format!("commit {ordinal:040x}\n    subject {ordinal}\n"));
    }

    fn append_file(input: &mut String, path: &str) {
        input.push_str(&format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n"
        ));
    }

    fn document_with_paths(commits: &[&[&str]]) -> DocumentSnapshot {
        let mut input = String::new();
        for (commit_index, paths) in commits.iter().enumerate() {
            append_commit(&mut input, u64::try_from(commit_index).unwrap() + 1);
            for path in *paths {
                append_file(&mut input, path);
            }
        }
        let storage = StructuralStorage::new().unwrap();
        parse_with_storage(input.as_bytes(), &storage)
    }

    fn generated_file_document(file_count: u64) -> DocumentSnapshot {
        let mut input = String::new();
        append_commit(&mut input, 1);
        for index in 0..file_count {
            append_file(&mut input, &format!("src/file-{index:05}.rs"));
        }
        let storage = StructuralStorage::new().unwrap();
        parse_with_storage(input.as_bytes(), &storage)
    }

    fn large_file_document() -> Arc<DocumentSnapshot> {
        static DOCUMENT: OnceLock<Arc<DocumentSnapshot>> = OnceLock::new();
        Arc::clone(DOCUMENT.get_or_init(|| Arc::new(generated_file_document(6_007))))
    }

    fn file_filter_task(
        query: &str,
        preferred_file: Option<FileId>,
        requested_rank: Option<u64>,
    ) -> FileFilterTask {
        FileFilterTask {
            generation: FileFilterGeneration::default(),
            document_generation: DocumentGeneration::default(),
            query: query.to_owned(),
            preferred_file,
            requested_rank,
        }
    }

    fn filter_result(document: &DocumentSnapshot, task: &FileFilterTask) -> FileFilterResult {
        let mut readers = FileFilterReaders::new();
        filter_file_paths(task, document, &mut readers, &|| false)
            .unwrap()
            .unwrap()
    }

    fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
        let started = Instant::now();
        while !predicate() {
            assert!(started.elapsed() < timeout, "test condition timed out");
            std::thread::sleep(Duration::from_millis(1));
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
    fn cancellation_sequence_evicts_queued_request_and_response() {
        let (request_tx, queued_requests) = bounded(1);
        let (response_tx, responses) = bounded(1);
        request_tx.try_send("request").unwrap();
        response_tx.try_send("response").unwrap();
        let next_sequence = AtomicU64::new(41);
        let latest_sequence = AtomicU64::new(7);
        let submission_gate = Mutex::new(());

        cancel_latest_work(
            &next_sequence,
            &latest_sequence,
            &submission_gate,
            &queued_requests,
            &responses,
        );

        assert_eq!(latest_sequence.load(Ordering::Acquire), 42);
        assert!(queued_requests.is_empty());
        assert!(responses.is_empty());
    }

    #[test]
    fn search_cancel_suppresses_old_work_and_accepts_a_later_submit() {
        let worker = SearchWorker::spawn().unwrap();
        let stale_input = osc_prefixed_line(MAX_EAGER_TEXT_RAW_BYTES * 2, b"alpha");
        let stale_document = Arc::new(document_snapshot_with_display_lengths(&stale_input, &[5]));
        let stale_snapshot = Arc::downgrade(&stale_document);

        assert!(worker.submit_latest(SearchJob {
            task: search_task(SearchGeneration::default(), "missing"),
            document: stale_document,
        }));
        worker.cancel_latest();

        wait_until(Duration::from_secs(1), || {
            stale_snapshot.upgrade().is_none()
        });
        assert!(
            worker
                .responses
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );

        let current_document = Arc::new(document_snapshot_with_display_lengths(b"beta\n", &[4]));
        assert!(worker.submit_latest(SearchJob {
            task: search_task(SearchGeneration::default(), "beta"),
            document: current_document,
        }));
        assert!(matches!(
            worker.responses.recv_timeout(Duration::from_secs(1)),
            Ok(SearchWorkerEvent::Completed {
                result: ScanResult { found: Some(_), .. },
                ..
            })
        ));
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

    #[test]
    fn file_filter_window_is_capped_and_centered_near_the_end() {
        let document = large_file_document();
        let task = file_filter_task("", None, Some(u64::MAX));
        let result = filter_result(&document, &task);

        assert_eq!(result.total_matches, 6_007);
        assert_eq!(result.selected_rank, Some(6_006));
        assert_eq!(result.start_rank, 1_007);
        assert_eq!(result.file_ids.len(), FILE_FILTER_WINDOW_CAPACITY);
        assert_eq!(result.file_ids.first(), Some(&FileId::new(1_007)));
        assert_eq!(result.file_ids.last(), Some(&FileId::new(6_006)));
    }

    #[test]
    fn file_filter_preserves_git_order_and_prefers_stable_identity() {
        let document =
            document_with_paths(&[&["z-last.rs", "a-first.rs"], &["middle.rs", "another.rs"]]);
        let task = file_filter_task(".rs", Some(FileId::new(2)), Some(0));
        let result = filter_result(&document, &task);

        assert_eq!(result.total_matches, 4);
        assert_eq!(result.selected_rank, Some(2));
        assert_eq!(
            result.file_ids,
            vec![
                FileId::new(0),
                FileId::new(1),
                FileId::new(2),
                FileId::new(3)
            ]
        );
    }

    #[test]
    fn file_filter_returns_an_empty_window_for_no_matches() {
        let document = document_with_paths(&[&["src/main.rs", "tests/main.rs"]]);
        let task = file_filter_task("not-present", Some(FileId::new(1)), Some(99));
        let result = filter_result(&document, &task);

        assert_eq!(result.total_matches, 0);
        assert_eq!(result.selected_rank, None);
        assert_eq!(result.start_rank, 0);
        assert!(result.file_ids.is_empty());
    }

    #[test]
    fn file_filter_rejects_a_query_above_its_product_allocation_limit() {
        let query = "x".repeat(MAX_FILE_FILTER_QUERY_BYTES + 1);
        let error = lowercase_query(&query, &|| false).unwrap_err();

        assert!(matches!(
            error,
            FileFilterError::QueryTooLong {
                actual,
                limit: MAX_FILE_FILTER_QUERY_BYTES,
            } if actual == MAX_FILE_FILTER_QUERY_BYTES + 1
        ));
    }

    #[test]
    fn lowercase_query_cancels_during_sizing_and_output_passes() {
        let query = "x".repeat(PATH_CANCELLATION_INTERVAL_CHARS * 2);
        for cancel_at in [2_usize, 3] {
            let checks = Cell::new(0_usize);
            let output = lowercase_query(&query, &|| {
                let next = checks.get().saturating_add(1);
                checks.set(next);
                next >= cancel_at
            })
            .unwrap();

            assert!(output.is_none());
            assert_eq!(checks.get(), cancel_at);
        }
    }

    #[test]
    fn file_filter_lowercases_unicode_and_matches_across_rename_paths() {
        let input = concat!(
            "commit 1111111111111111111111111111111111111111\n",
            "diff --git a/Älter.rs b/Über.rs\n",
            "similarity index 90%\n",
            "rename from Älter.rs\n",
            "rename to Über.rs\n",
        );
        let storage = StructuralStorage::new().unwrap();
        let document = parse_with_storage(input.as_bytes(), &storage);
        // Queryは旧pathの`Äl`から新pathの`Ü`へseparatorをまたぐsubsequenceになる。
        let task = file_filter_task("ÄLÜ", None, None);
        let result = filter_result(&document, &task);

        assert_eq!(result.total_matches, 1);
        assert_eq!(result.file_ids, vec![FileId::new(0)]);
    }

    #[test]
    fn file_filter_reads_paths_after_structural_records_and_blobs_spill() {
        let limit = 512 * 1024;
        let config = StorageConfig::new(limit, 480 * 1024, 256, 4 * 1024).unwrap();
        let storage = StructuralStorage::with_config(config).unwrap();
        let mut input = String::new();
        append_commit(&mut input, 1);
        for index in 0..800_u64 {
            append_file(
                &mut input,
                &format!("spill/{index:04}/component/a-long-source-file-name.rs"),
            );
        }
        let document = parse_with_storage(input.as_bytes(), &storage);
        let task = file_filter_task("0799component", None, None);
        let result = filter_result(&document, &task);

        assert_eq!(result.total_matches, 1);
        assert_eq!(result.file_ids, vec![FileId::new(799)]);
        assert!(storage.resident_bytes().unwrap().total <= limit);
    }

    #[test]
    fn long_path_matcher_observes_cancellation_after_bounded_path_decode() {
        let long_path = format!("src/{}.rs", "a".repeat(16 * 1024));
        let document = document_with_paths(&[&[long_path.as_str()]]);
        let task = file_filter_task("zzzz", None, None);
        let checks = Cell::new(0_usize);
        let mut readers = FileFilterReaders::new();
        let result = filter_file_paths(&task, &document, &mut readers, &|| {
            let next = checks.get().saturating_add(1);
            checks.set(next);
            next >= 3
        })
        .unwrap();

        assert!(result.is_none());
        assert_eq!(checks.get(), 3);
    }

    #[test]
    fn file_filter_worker_keeps_one_pending_job_and_one_response() {
        let worker = FileFilterWorker::spawn().unwrap();

        assert_eq!(worker.requests.capacity(), Some(1));
        assert_eq!(worker.responses.capacity(), Some(1));
    }

    #[test]
    fn file_filter_cancel_suppresses_old_work_and_accepts_a_later_submit() {
        let worker = FileFilterWorker::spawn().unwrap();
        let stale_document = Arc::new(generated_file_document(6_007));
        let stale_snapshot = Arc::downgrade(&stale_document);

        assert!(worker.submit_latest(FileFilterJob {
            task: file_filter_task("definitely-missing", None, None),
            document: stale_document,
        }));
        worker.cancel_latest();

        wait_until(Duration::from_secs(2), || {
            stale_snapshot.upgrade().is_none()
        });
        assert!(
            worker
                .responses
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );

        let current_document = Arc::new(document_with_paths(&[&[
            "current/alpha.rs",
            "current/target.rs",
        ]]));
        assert!(worker.submit_latest(FileFilterJob {
            task: file_filter_task("target", None, None),
            document: current_document,
        }));
        assert!(matches!(
            worker.responses.recv_timeout(Duration::from_secs(1)),
            Ok(FileFilterWorkerEvent::Completed(FileFilterResult {
                total_matches: 1,
                ref file_ids,
                ..
            })) if file_ids == &[FileId::new(1)]
        ));
    }

    #[test]
    fn file_filter_worker_converts_a_typed_scan_error_to_failure() {
        let worker = FileFilterWorker::spawn().unwrap();
        let document = Arc::new(document_with_paths(&[&["src/main.rs"]]));
        let task = file_filter_task(&"x".repeat(MAX_FILE_FILTER_QUERY_BYTES + 1), None, None);
        let generation = task.generation;
        let document_generation = task.document_generation;
        assert!(worker.submit_latest(FileFilterJob { task, document }));

        let response = worker
            .responses
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(matches!(
            response,
            FileFilterWorkerEvent::Failed {
                generation: actual_generation,
                document_generation: actual_document_generation,
                ref message,
            } if actual_generation == generation
                && actual_document_generation == document_generation
                && message.contains("exceeding")
        ));
    }

    #[test]
    fn file_filter_worker_replacement_cancels_or_suppresses_stale_scan() {
        let worker = FileFilterWorker::spawn().unwrap();
        let stale_document = large_file_document();
        let current_document = Arc::new(document_with_paths(&[&[
            "current/alpha.rs",
            "current/target.rs",
        ]]));

        assert!(worker.submit_latest(FileFilterJob {
            task: file_filter_task("definitely-missing", None, None),
            document: stale_document,
        }));
        std::thread::sleep(Duration::from_millis(1));
        assert!(worker.submit_latest(FileFilterJob {
            task: file_filter_task("target", None, None),
            document: current_document,
        }));

        let response = worker
            .responses
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert!(matches!(
            response,
            FileFilterWorkerEvent::Completed(FileFilterResult {
                total_matches: 1,
                ref file_ids,
                ..
            }) if file_ids == &[FileId::new(1)]
        ));
        assert!(
            worker
                .responses
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );
    }

    #[test]
    fn file_filter_submit_removes_an_already_published_stale_response() {
        let worker = FileFilterWorker::spawn().unwrap();
        let stale_document = Arc::new(document_with_paths(&[&["stale.rs"]]));
        let current_document = Arc::new(document_with_paths(&[&["one.rs", "two.rs"]]));
        assert!(worker.submit_latest(FileFilterJob {
            task: file_filter_task("", None, None),
            document: stale_document,
        }));

        for _ in 0..1_000 {
            if !worker.responses.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!worker.responses.is_empty());
        assert!(worker.submit_latest(FileFilterJob {
            task: file_filter_task("", None, None),
            document: current_document,
        }));

        let response = worker
            .responses
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(matches!(
            response,
            FileFilterWorkerEvent::Completed(FileFilterResult {
                total_matches: 2,
                ..
            })
        ));
        assert!(
            worker
                .responses
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );
    }
}
