use std::cell::Cell;
use std::num::NonZeroUsize;

use git_ultrapager::search::{
    CommitIncrementalError, IncrementalQueryStatus, RepeatCommand, ScanOutcome, ScanRequest,
    ScanResult, SearchAnchor, SearchDirection, SearchHistory, SearchLine, SearchLineRead,
    SearchSession, SearchSource, compile_smart_case, repeat_anchor, scan_lines, scan_source,
};
use git_ultrapager::text::MAX_SEARCH_LINE_BYTES;

fn count(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("test count is non-zero")
}

struct CountingSource {
    lines: Vec<&'static str>,
    accesses: Cell<usize>,
    fail_at: Option<usize>,
}

impl SearchSource<usize> for CountingSource {
    type Error = &'static str;

    fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn read_line(
        &mut self,
        index: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<SearchLineRead<'_, usize>, Self::Error> {
        if cancelled() {
            return Ok(SearchLineRead::Cancelled);
        }
        self.accesses.set(self.accesses.get().saturating_add(1));
        if self.fail_at == Some(index) {
            return Err("page read failed");
        }
        let line = self
            .lines
            .get(index)
            .copied()
            .map(|text| SearchLine::new(index, text))
            .ok_or("line missing")?;
        if line.text.len() > MAX_SEARCH_LINE_BYTES {
            Ok(SearchLineRead::Oversized { line_id: line.id })
        } else {
            Ok(SearchLineRead::Searchable(line))
        }
    }
}

struct ReusingBufferSource {
    line_count: usize,
    accesses: usize,
    buffer: String,
}

struct ClassifiedSource {
    lines: Vec<Option<&'static str>>,
    accesses: Vec<usize>,
}

impl SearchSource<usize> for ClassifiedSource {
    type Error = ();

    fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn read_line(
        &mut self,
        index: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<SearchLineRead<'_, usize>, Self::Error> {
        if cancelled() {
            return Ok(SearchLineRead::Cancelled);
        }
        let access = self.accesses.get_mut(index).ok_or(())?;
        *access = access.saturating_add(1);
        match self.lines.get(index).ok_or(())? {
            Some(text) => Ok(SearchLineRead::Searchable(SearchLine::new(index, text))),
            None => Ok(SearchLineRead::Oversized { line_id: index }),
        }
    }
}

impl SearchSource<usize> for ReusingBufferSource {
    type Error = ();

    fn line_count(&self) -> usize {
        self.line_count
    }

    fn read_line(
        &mut self,
        index: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<SearchLineRead<'_, usize>, Self::Error> {
        if cancelled() {
            return Ok(SearchLineRead::Cancelled);
        }
        if index >= self.line_count {
            return Err(());
        }
        self.accesses = self.accesses.saturating_add(1);
        self.buffer.clear();
        self.buffer
            .push_str(if index == 0 { "needle" } else { "haystack" });
        Ok(SearchLineRead::Searchable(SearchLine::new(
            index,
            &self.buffer,
        )))
    }
}

#[test]
fn smart_case_uses_unicode_uppercase_as_the_switch() {
    let insensitive = compile_smart_case("äpfel").expect("valid regex");
    assert!(!insensitive.is_case_sensitive());
    assert!(insensitive.regex().is_match("ÄPFEL"));

    let sensitive = compile_smart_case("Äpfel").expect("valid regex");
    assert!(sensitive.is_case_sensitive());
    assert!(sensitive.regex().is_match("Äpfel"));
    assert!(!sensitive.regex().is_match("äpfel"));
}

#[test]
fn scanning_walks_matches_in_both_document_directions() {
    let compiled = compile_smart_case("foo").expect("valid regex");
    let lines = [
        SearchLine::new(10_u64, "foo and foo"),
        SearchLine::new(20_u64, "nothing"),
        SearchLine::new(30_u64, "Foo"),
    ];

    let forward = scan_lines(
        &compiled,
        &lines,
        ScanRequest::new(
            SearchDirection::Forward,
            SearchAnchor::BeforeDocument,
            count(3),
        ),
    );
    let found = forward.found.expect("third match");
    assert_eq!(found.line_id, 30);
    assert_eq!(found.line_index, 2);
    assert_eq!(found.byte_range, 0..3);

    let backward = scan_lines(
        &compiled,
        &lines,
        ScanRequest::new(
            SearchDirection::Backward,
            SearchAnchor::AfterDocument,
            count(2),
        ),
    );
    let found = backward.found.expect("second match in reverse order");
    assert_eq!(found.line_id, 10);
    assert_eq!(found.byte_range, 8..11);
}

#[test]
fn accessor_scan_cancels_before_reading_the_remaining_lines() {
    let compiled = compile_smart_case("missing").expect("valid regex");
    let mut source = CountingSource {
        lines: vec!["haystack"; 100],
        accesses: Cell::new(0),
        fail_at: None,
    };
    let checks = Cell::new(0_usize);
    let outcome = scan_source(
        &compiled,
        &mut source,
        ScanRequest::new(
            SearchDirection::Forward,
            SearchAnchor::BeforeDocument,
            count(1),
        ),
        &|| {
            let next = checks.get().saturating_add(1);
            checks.set(next);
            next > 8
        },
    );

    assert!(matches!(outcome, ScanOutcome::Cancelled));
    assert!(source.accesses.get() < source.lines.len());
}

#[test]
fn accessor_scan_does_not_preload_a_large_virtual_document() {
    let compiled = compile_smart_case("needle").expect("valid regex");
    let mut source = ReusingBufferSource {
        line_count: 5_000_000,
        accesses: 0,
        buffer: String::new(),
    };
    let outcome = scan_source(
        &compiled,
        &mut source,
        ScanRequest::new(
            SearchDirection::Forward,
            SearchAnchor::BeforeDocument,
            count(1),
        ),
        &|| false,
    );

    assert!(matches!(
        outcome,
        ScanOutcome::Completed(ScanResult { found: Some(_), .. })
    ));
    assert_eq!(source.accesses, 1);
}

#[test]
fn anchor_line_is_read_once_in_each_direction() {
    let compiled = compile_smart_case("needle").expect("valid regex");
    let mut forward = ClassifiedSource {
        lines: vec![Some("before"), Some("needle")],
        accesses: vec![0; 2],
    };
    let outcome = scan_source(
        &compiled,
        &mut forward,
        ScanRequest::new(SearchDirection::Forward, SearchAnchor::at(1, 0), count(1)),
        &|| false,
    );
    assert!(matches!(
        outcome,
        ScanOutcome::Completed(ScanResult { found: Some(_), .. })
    ));
    assert_eq!(forward.accesses, vec![0, 1]);

    let mut backward = ClassifiedSource {
        lines: vec![Some("before"), Some("needle")],
        accesses: vec![0; 2],
    };
    let outcome = scan_source(
        &compiled,
        &mut backward,
        ScanRequest::new(
            SearchDirection::Backward,
            SearchAnchor::AfterDocument,
            count(1),
        ),
        &|| false,
    );
    assert!(matches!(
        outcome,
        ScanOutcome::Completed(ScanResult { found: Some(_), .. })
    ));
    assert_eq!(backward.accesses, vec![0, 1]);
}

#[test]
fn oversized_anchor_is_skipped_once_in_both_directions() {
    let compiled = compile_smart_case("needle").expect("valid regex");
    let mut forward = ClassifiedSource {
        lines: vec![Some("before"), None, Some("needle")],
        accesses: vec![0; 3],
    };
    let outcome = scan_source(
        &compiled,
        &mut forward,
        ScanRequest::new(SearchDirection::Forward, SearchAnchor::at(1, 7), count(1)),
        &|| false,
    );
    let ScanOutcome::Completed(result) = outcome else {
        panic!("forward scan completed");
    };
    assert_eq!(result.skipped_oversized_lines, 1);
    assert_eq!(
        result.found.expect("match after oversized anchor").line_id,
        2
    );
    assert_eq!(forward.accesses, vec![0, 1, 1]);

    let mut backward = ClassifiedSource {
        lines: vec![Some("needle"), None, Some("after")],
        accesses: vec![0; 3],
    };
    let outcome = scan_source(
        &compiled,
        &mut backward,
        ScanRequest::new(
            SearchDirection::Backward,
            SearchAnchor::at(1, usize::MAX),
            count(1),
        ),
        &|| false,
    );
    let ScanOutcome::Completed(result) = outcome else {
        panic!("backward scan completed");
    };
    assert_eq!(result.skipped_oversized_lines, 1);
    assert_eq!(
        result.found.expect("match before oversized anchor").line_id,
        0
    );
    assert_eq!(backward.accesses, vec![1, 1, 0]);
}

#[test]
fn accessor_failure_is_distinct_from_generation_cancellation() {
    let compiled = compile_smart_case("needle").expect("valid regex");
    let mut source = CountingSource {
        lines: vec!["first", "needle"],
        accesses: Cell::new(0),
        fail_at: Some(1),
    };
    let outcome = scan_source(
        &compiled,
        &mut source,
        ScanRequest::new(
            SearchDirection::Forward,
            SearchAnchor::BeforeDocument,
            count(1),
        ),
        &|| false,
    );

    assert_eq!(outcome, ScanOutcome::Failed("page read failed"));
}

#[test]
fn zero_width_repeats_advance_by_a_unicode_scalar() {
    let compiled = compile_smart_case(r"\b").expect("valid regex");
    let lines = [SearchLine::new(7_u64, "界a")];

    let first = scan_lines(
        &compiled,
        &lines,
        ScanRequest::new(
            SearchDirection::Forward,
            SearchAnchor::BeforeDocument,
            count(1),
        ),
    )
    .found
    .expect("first boundary");
    assert_eq!(first.byte_range, 0..0);

    let next_anchor = repeat_anchor(&first, SearchDirection::Forward, &lines);
    assert_eq!(next_anchor, SearchAnchor::at(0, "界".len()));
    let second = scan_lines(
        &compiled,
        &lines,
        ScanRequest::new(SearchDirection::Forward, next_anchor, count(1)),
    )
    .found
    .expect("last boundary");
    assert_eq!(second.byte_range, "界a".len().."界a".len());

    let previous_anchor = repeat_anchor(&second, SearchDirection::Backward, &lines);
    assert_eq!(previous_anchor, SearchAnchor::at(0, "界".len()));
    let previous = scan_lines(
        &compiled,
        &lines,
        ScanRequest::new(SearchDirection::Backward, previous_anchor, count(1)),
    )
    .found
    .expect("first boundary again");
    assert_eq!(previous.byte_range, 0..0);
}

#[test]
fn oversized_lines_are_skipped_and_counted() {
    let compiled = compile_smart_case("needle").expect("valid regex");
    let oversized = "x".repeat(MAX_SEARCH_LINE_BYTES + 1);
    let lines = [
        SearchLine::new(1_u8, oversized.as_str()),
        SearchLine::new(2_u8, "needle"),
    ];

    let result = scan_lines(
        &compiled,
        &lines,
        ScanRequest::new(
            SearchDirection::Forward,
            SearchAnchor::BeforeDocument,
            count(1),
        ),
    );

    assert_eq!(result.skipped_oversized_lines, 1);
    assert_eq!(result.found.expect("match after skipped line").line_id, 2);
}

#[test]
fn zero_width_scan_selects_each_match_without_collecting_line_results() {
    let compiled = compile_smart_case(r"^|$").expect("valid regex");
    let lines = [SearchLine::new(9_u8, "abc")];

    let first = scan_lines(
        &compiled,
        &lines,
        ScanRequest::new(
            SearchDirection::Forward,
            SearchAnchor::BeforeDocument,
            count(1),
        ),
    );
    let second = scan_lines(
        &compiled,
        &lines,
        ScanRequest::new(
            SearchDirection::Forward,
            SearchAnchor::BeforeDocument,
            count(2),
        ),
    );

    assert_eq!(
        first.found.expect("first zero-width match").byte_range,
        0..0
    );
    assert_eq!(
        second.found.expect("second zero-width match").byte_range,
        3..3
    );
}

#[test]
fn distant_zero_width_match_keeps_the_public_scan_result_fixed_size() {
    // 入力長に比例するmatch数でも、public境界が保持するのは指定ordinalの1件だけにする。
    let text = "a".repeat(1 << 16);
    let compiled = compile_smart_case("").expect("valid zero-width regex");
    let lines = [SearchLine::new(5_u8, text.as_str())];
    let result = scan_lines(
        &compiled,
        &lines,
        ScanRequest::new(
            SearchDirection::Backward,
            SearchAnchor::AfterDocument,
            count(text.len() + 1),
        ),
    );

    let ScanResult {
        found,
        skipped_oversized_lines,
        forward_resume,
    } = result;
    let found = found.expect("first boundary in reverse ordinal order");
    assert_eq!(found.line_id, 5);
    assert_eq!(found.byte_range, 0..0);
    assert_eq!(skipped_oversized_lines, 0);
    assert_eq!(forward_resume, None);
}

#[test]
fn history_restores_the_draft_after_newest_entry() {
    let mut history = SearchHistory::default();
    history.record("alpha");
    history.record("beta");

    assert_eq!(history.older("draft"), Some("beta".to_owned()));
    assert_eq!(
        history.older("ignored while browsing"),
        Some("alpha".to_owned())
    );
    assert_eq!(history.newer(), Some("beta".to_owned()));
    assert_eq!(history.newer(), Some("draft".to_owned()));
    assert_eq!(history.newer(), None);
}

#[test]
fn invalid_incremental_regex_keeps_the_last_valid_preview() {
    let mut session = SearchSession::<u64>::default();
    let generation = session.begin_incremental(SearchDirection::Forward, SearchAnchor::at(0, 0));
    let update = session.update_incremental("foo").expect("incremental mode");
    assert!(matches!(update.status, IncrementalQueryStatus::Valid));
    assert_ne!(update.generation, generation);

    let lines = [SearchLine::new(4_u64, "foo")];
    let preview = scan_lines(
        session
            .incremental()
            .and_then(|query| query.compiled_for_current_query())
            .expect("compiled current query"),
        &lines,
        ScanRequest::new(
            SearchDirection::Forward,
            SearchAnchor::BeforeDocument,
            count(1),
        ),
    );
    assert!(session.apply_incremental_preview(update.generation, preview));

    let invalid = session.update_incremental("[").expect("incremental mode");
    assert!(matches!(invalid.status, IncrementalQueryStatus::Invalid(_)));
    let retained = session
        .incremental()
        .and_then(|query| query.displayed_preview())
        .and_then(|preview| preview.found.as_ref())
        .expect("last valid preview remains visible");
    assert_eq!(retained.byte_range, 0..3);
    assert_eq!(
        session
            .incremental()
            .and_then(|query| query.displayed_compiled())
            .map(|compiled| compiled.pattern()),
        Some("foo")
    );

    let _ = session.update_incremental("").expect("incremental mode");
    assert!(
        session
            .incremental()
            .and_then(|query| query.displayed_preview())
            .is_none()
    );
}

#[test]
fn committing_a_preview_preserves_its_oversized_line_count() {
    let mut session = SearchSession::<u64>::default();
    session.begin_incremental(SearchDirection::Forward, SearchAnchor::BeforeDocument);
    let update = session.update_incremental("foo").expect("incremental mode");
    assert!(session.apply_incremental_preview(
        update.generation,
        ScanResult {
            found: None,
            skipped_oversized_lines: 7,
            forward_resume: None,
        },
    ));

    let committed = session.commit_incremental().expect("valid query");
    assert_eq!(
        committed
            .preview
            .as_ref()
            .map(|preview| preview.skipped_oversized_lines),
        Some(7)
    );
    assert_eq!(session.last_skipped_oversized_lines(), 7);
}

#[test]
fn empty_incremental_query_reuses_the_pattern_in_the_new_direction() {
    let mut session = SearchSession::<u64>::default();
    session.activate(
        compile_smart_case("saved").expect("valid regex"),
        SearchDirection::Forward,
    );
    let history_len = session.history().entries().len();

    session.begin_incremental(SearchDirection::Backward, SearchAnchor::at(2, 4));
    let committed = session
        .commit_incremental()
        .expect("previous search exists");

    assert!(committed.reused_previous_pattern);
    assert_eq!(committed.compiled.pattern(), "saved");
    assert_eq!(committed.direction, SearchDirection::Backward);
    assert_eq!(committed.origin, SearchAnchor::at(2, 4));
    assert_eq!(session.history().entries().len(), history_len);
    assert_eq!(
        session.direction_for_repeat(RepeatCommand::Next),
        Some(SearchDirection::Backward)
    );
}

#[test]
fn empty_incremental_query_without_history_stays_open() {
    let mut session = SearchSession::<u64>::default();
    session.begin_incremental(SearchDirection::Forward, SearchAnchor::BeforeDocument);

    assert!(matches!(
        session.commit_incremental(),
        Err(CommitIncrementalError::NoPreviousSearch)
    ));
    assert!(session.incremental().is_some());
}

#[test]
fn repeat_direction_is_relative_to_the_committed_search() {
    let mut session = SearchSession::<u64>::default();
    let compiled = compile_smart_case("foo").expect("valid regex");
    session.activate(compiled, SearchDirection::Backward);

    assert_eq!(
        session.direction_for_repeat(RepeatCommand::Next),
        Some(SearchDirection::Backward)
    );
    assert_eq!(
        session.direction_for_repeat(RepeatCommand::Opposite),
        Some(SearchDirection::Forward)
    );
}

#[test]
fn a_new_generation_cancels_pending_forward_and_rejects_stale_results() {
    let mut session = SearchSession::<u64>::default();
    session.activate(
        compile_smart_case("later").expect("valid regex"),
        SearchDirection::Forward,
    );
    let generation = session.begin_navigation_intent();
    let loaded = [SearchLine::new(1_u64, "not yet")];
    let exhausted = scan_lines(
        session.active().expect("active search").compiled(),
        &loaded,
        ScanRequest::new(
            SearchDirection::Forward,
            SearchAnchor::BeforeDocument,
            count(1),
        ),
    );
    assert!(session.apply_scan_result(generation, exhausted, false));
    assert_eq!(
        session
            .pending_forward()
            .expect("waiting for incoming data")
            .next_line_index,
        1
    );

    let replacement = session.begin_navigation_intent();
    assert_ne!(replacement, generation);
    assert!(session.pending_forward().is_none());

    let appended = [
        SearchLine::new(1_u64, "not yet"),
        SearchLine::new(2_u64, "later"),
    ];
    let stale = scan_lines(
        session.active().expect("active search").compiled(),
        &appended,
        ScanRequest::new(SearchDirection::Forward, SearchAnchor::at(1, 0), count(1)),
    );
    assert!(!session.apply_scan_result(generation, stale, false));
    assert!(session.current_match().is_none());
}

#[test]
fn pending_forward_preserves_remaining_count_across_appends() {
    let mut session = SearchSession::<u64>::default();
    session.activate(
        compile_smart_case("hit").expect("valid regex"),
        SearchDirection::Forward,
    );
    let generation = session.begin_navigation_intent();
    let first_chunk = [SearchLine::new(1_u64, "hit")];
    let first_result = scan_lines(
        session.active().expect("active search").compiled(),
        &first_chunk,
        ScanRequest::new(
            SearchDirection::Forward,
            SearchAnchor::BeforeDocument,
            count(3),
        ),
    );
    assert!(session.apply_scan_result(generation, first_result, false));
    let pending = *session.pending_forward().expect("two matches remain");
    assert_eq!(pending.remaining_count, count(2));

    let second_chunk = [
        SearchLine::new(1_u64, "hit"),
        SearchLine::new(2_u64, "hit hit"),
    ];
    let resumed = scan_lines(
        session.active().expect("active search").compiled(),
        &second_chunk,
        pending.request(),
    );
    assert!(session.apply_scan_result(generation, resumed, false));
    let found = session.current_match().expect("third match arrived");
    assert_eq!(found.line_id, 2);
    assert_eq!(found.byte_range, 4..7);
    assert!(session.pending_forward().is_none());
}

#[test]
fn backward_exhaustion_never_enters_pending_state() {
    let compiled = compile_smart_case("missing").expect("valid regex");
    let lines = [SearchLine::new(1_u8, "text")];
    let result = scan_lines(
        &compiled,
        &lines,
        ScanRequest::new(
            SearchDirection::Backward,
            SearchAnchor::AfterDocument,
            count(1),
        ),
    );

    assert!(result.found.is_none());
    assert!(result.forward_resume.is_none());
}
