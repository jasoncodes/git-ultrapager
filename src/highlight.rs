//! File type detection と syntax highlighting actor。
//!
//! `syntect::ParseState` / `HighlightState` は worker 間を移動させず、専用 actor thread が所有する。
//! AppState へ返すのは thread-independent な byte span と RGB/style bit だけである。diff background、
//! search、intraline の precedence は renderer が合成し、この module は source foreground だけを
//! 提供する。

use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use thiserror::Error;

use crate::cli::ThemeChoice;
use crate::text::MAX_SYNTAX_LINE_BYTES;

/// Syntax style を renderer へ渡す immutable span。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntaxSpan {
    /// Request line のoriginal UTF-8 byte range。synthetic newlineは含まない。
    pub byte_range: Range<usize>,
    pub foreground: (u8, u8, u8),
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// 1 logical source line の highlight 結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HighlightedLine {
    Styled(Vec<SyntaxSpan>),
    Plain,
}

/// Syntax actor へ渡す hunk 単位の request。
#[derive(Clone, Debug)]
pub struct SyntaxRequest {
    /// Reflow/document replacement後のstale responseを捨てるための世代。
    pub generation: u64,
    /// 同一世代内でrequestとresponseを対応付けるID。
    pub request_id: u64,
    /// File type判定だけに使うGit由来のpath。repository fileは開かない。
    pub path: Option<String>,
    /// Pathで判定できない場合の追加/context側先頭行。shebang以外は判定に使わない。
    pub first_line: Option<String>,
    /// 同じside・hunkに属し、先頭からparse stateを連続させるlogical line列。
    pub lines: Arc<[String]>,
    pub theme: ThemeChoice,
}

/// Syntax actor から main state へ返す result。
#[derive(Clone, Debug)]
pub struct SyntaxResponse {
    /// Requestから複写し、main state側でstale判定に使う世代。
    pub generation: u64,
    /// Requestから複写した対応ID。
    pub request_id: u64,
    /// Line順のspan列。worker panicは`Err`となり、actor自体は次のrequestを受け付ける。
    pub result: Result<Vec<HighlightedLine>, HighlightError>,
}

/// Highlight setup または grammar evaluation の failure。
#[derive(Clone, Debug, Error)]
pub enum HighlightError {
    #[error("built-in syntax set does not contain Plain Text")]
    MissingPlainText,
    #[error("built-in theme `{0}` is unavailable")]
    MissingTheme(&'static str),
    #[error("syntax highlighting failed: {0}")]
    Syntect(String),
    #[error("syntax worker could not start: {0}")]
    WorkerSpawn(String),
    #[error("syntax worker panicked")]
    WorkerPanicked,
}

/// 専用 syntax thread の request handle。
pub struct SyntaxWorker {
    requests: Sender<SyntaxRequest>,
    pub responses: Receiver<SyntaxResponse>,
}

impl SyntaxWorker {
    /// Bounded queue を持つ actor を開始する。
    ///
    /// OS がthreadを作れない場合はhandleを返さず、callerがinteractive起動をerrorへ写せる。queueが
    /// 満杯の場合は[`Self::try_submit`]がrequestを返し、callerがviewport優先順位を更新する。
    pub fn spawn(queue_capacity: usize) -> Result<Self, HighlightError> {
        let (request_tx, request_rx) = bounded::<SyntaxRequest>(queue_capacity.max(1));
        let (response_tx, response_rx) = bounded::<SyntaxResponse>(queue_capacity.max(1));
        thread::Builder::new()
            .name("git-ultrapager-syntax".to_owned())
            .spawn(move || syntax_actor(request_rx, response_tx))
            .map_err(|error| HighlightError::WorkerSpawn(error.to_string()))?;
        Ok(Self {
            requests: request_tx,
            responses: response_rx,
        })
    }

    /// Request を待たずにenqueueする。
    ///
    /// Queue満杯またはworker切断時は所有権をcallerへ返す。callerは待機せず、次のviewport requestを
    /// 優先するか、切断をtask failureとして扱う。
    pub fn try_submit(&self, request: SyntaxRequest) -> Result<(), SyntaxRequest> {
        match self.requests.try_send(request) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(request) | TrySendError::Disconnected(request)) => Err(request),
        }
    }
}

fn syntax_actor(requests: Receiver<SyntaxRequest>, responses: Sender<SyntaxResponse>) {
    let engine =
        std::panic::catch_unwind(SyntaxEngine::new).unwrap_or(Err(HighlightError::WorkerPanicked));
    while let Ok(request) = requests.recv() {
        let generation = request.generation;
        let request_id = request.request_id;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            engine
                .as_ref()
                .map_err(|error| error.clone())
                .and_then(|engine| engine.highlight(&request))
        }))
        .unwrap_or(Err(HighlightError::WorkerPanicked));
        if responses
            .send(SyntaxResponse {
                generation,
                request_id,
                result,
            })
            .is_err()
        {
            break;
        }
    }
}

pub(crate) struct SyntaxEngine {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
}

impl SyntaxEngine {
    pub(crate) fn new() -> Result<Self, HighlightError> {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        if syntaxes.find_syntax_by_name("Plain Text").is_none() {
            return Err(HighlightError::MissingPlainText);
        }
        Ok(Self {
            syntaxes,
            themes: ThemeSet::load_defaults(),
        })
    }

    pub(crate) fn session<'engine>(
        &'engine self,
        path: Option<&str>,
        first_line: Option<&str>,
        theme_choice: ThemeChoice,
    ) -> Result<Option<SyntaxSession<'engine>>, HighlightError> {
        let Some(syntax) = detect_syntax(&self.syntaxes, path, first_line) else {
            return Ok(None);
        };
        let theme_name = match theme_choice {
            ThemeChoice::Dark => "base16-ocean.dark",
            ThemeChoice::Light => "InspiredGitHub",
        };
        let theme = self
            .themes
            .themes
            .get(theme_name)
            .ok_or(HighlightError::MissingTheme(theme_name))?;
        Ok(Some(SyntaxSession {
            syntaxes: &self.syntaxes,
            syntax,
            theme,
            highlighter: HighlightLines::new(syntax, theme),
        }))
    }

    fn highlight(&self, request: &SyntaxRequest) -> Result<Vec<HighlightedLine>, HighlightError> {
        let mut session = self.session(
            request.path.as_deref(),
            request.first_line.as_deref(),
            request.theme,
        )?;
        let mut output = Vec::with_capacity(request.lines.len());

        for line in request.lines.iter() {
            output.push(match session.as_mut() {
                Some(session) => session.highlight_line(line)?,
                None => HighlightedLine::Plain,
            });
        }
        Ok(output)
    }
}

/// 1 file side・1 hunk の連続したparse state。
///
/// Decoration計算はold/new用に2つ作り、deletionはoldだけ、additionはnewだけ、contextは両方へ
/// 順番どおり渡す。Hunkごとに作り直すことで、別hunkの不完全なsyntax stateを持ち越さない。
pub(crate) struct SyntaxSession<'engine> {
    syntaxes: &'engine SyntaxSet,
    syntax: &'engine syntect::parsing::SyntaxReference,
    theme: &'engine syntect::highlighting::Theme,
    highlighter: HighlightLines<'engine>,
}

impl SyntaxSession<'_> {
    /// Logical source lineを1行進め、original content byteへのspanを返す。
    pub(crate) fn highlight_line(&mut self, line: &str) -> Result<HighlightedLine, HighlightError> {
        if line.len() > MAX_SYNTAX_LINE_BYTES {
            // Parseしなかったlineがmultiline string/commentを開閉したかは分からない。直前stateを
            // 継続すると既知でないstateを既知として扱うため、次のlineはhunk先頭と同じstateへ戻す。
            self.highlighter = HighlightLines::new(self.syntax, self.theme);
            return Ok(HighlightedLine::Plain);
        }

        // `load_defaults_newlines` のgrammarはline末尾の`\n`でsingle-line contextをpopする。
        // Modelは改行をlogical lineから分離するため、worker内だけsynthetic newlineを補い、返す
        // spanはoriginal byte lengthでclipする。
        let mut line_with_newline = String::with_capacity(line.len() + 1);
        line_with_newline.push_str(line);
        line_with_newline.push('\n');
        let regions = self
            .highlighter
            .highlight_line(&line_with_newline, self.syntaxes)
            .map_err(|error| HighlightError::Syntect(error.to_string()))?;
        let mut offset = 0_usize;
        let spans = regions
            .into_iter()
            .filter_map(|(style, text)| {
                let start = offset;
                offset += text.len();
                let end = offset.min(line.len());
                (start < end).then_some(SyntaxSpan {
                    byte_range: start..end,
                    foreground: (style.foreground.r, style.foreground.g, style.foreground.b),
                    bold: style.font_style.contains(FontStyle::BOLD),
                    italic: style.font_style.contains(FontStyle::ITALIC),
                    underline: style.font_style.contains(FontStyle::UNDERLINE),
                })
            })
            .collect();
        Ok(HighlightedLine::Styled(spans))
    }
}

fn detect_syntax<'a>(
    syntaxes: &'a SyntaxSet,
    path: Option<&str>,
    first_line: Option<&str>,
) -> Option<&'a syntect::parsing::SyntaxReference> {
    let path_match = path.and_then(|path| {
        let path = Path::new(path);
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        syntaxes
            .find_syntax_by_extension(file_name)
            .or_else(|| syntaxes.find_syntax_by_extension(extension))
    });
    path_match.or_else(|| {
        first_line
            .filter(|line| line.len() <= MAX_SYNTAX_LINE_BYTES && line.starts_with("#!"))
            .and_then(|line| syntaxes.find_syntax_by_first_line(line))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_detection_does_not_read_repository_files() {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let syntax = detect_syntax(&syntaxes, Some("not/on/disk/main.rs"), None).unwrap();
        assert_eq!(syntax.name, "Rust");
    }

    #[test]
    fn shebang_is_used_only_as_fallback() {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let syntax =
            detect_syntax(&syntaxes, Some("script"), Some("#!/usr/bin/env python3")).unwrap();
        assert!(syntax.name.contains("Python"));
    }

    #[test]
    fn non_shebang_first_line_does_not_guess_a_syntax() {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        assert!(
            detect_syntax(&syntaxes, Some("unknown"), Some("<?xml version=\"1.0\"?>")).is_none()
        );
    }

    #[test]
    fn oversized_lines_fall_back_to_plain() {
        let engine = SyntaxEngine::new().unwrap();
        let request = SyntaxRequest {
            generation: 1,
            request_id: 1,
            path: Some("large.rs".to_owned()),
            first_line: None,
            lines: Arc::from(["x".repeat(MAX_SYNTAX_LINE_BYTES + 1)]),
            theme: ThemeChoice::Dark,
        };
        assert_eq!(
            engine.highlight(&request).unwrap(),
            vec![HighlightedLine::Plain]
        );
    }

    #[test]
    fn oversized_line_resets_unknown_parse_state() {
        let engine = SyntaxEngine::new().unwrap();
        let request = SyntaxRequest {
            generation: 1,
            request_id: 1,
            path: Some("large.rs".to_owned()),
            first_line: None,
            lines: Arc::from([
                "/* open comment".to_owned(),
                "x".repeat(MAX_SYNTAX_LINE_BYTES + 1),
                "let value = 1;".to_owned(),
            ]),
            theme: ThemeChoice::Dark,
        };
        let output = engine.highlight(&request).unwrap();

        let standalone = SyntaxRequest {
            generation: 2,
            request_id: 2,
            path: Some("large.rs".to_owned()),
            first_line: None,
            lines: Arc::from(["let value = 1;".to_owned()]),
            theme: ThemeChoice::Dark,
        };
        assert_eq!(output[2], engine.highlight(&standalone).unwrap()[0]);
    }

    #[test]
    fn synthetic_newline_closes_single_line_context() {
        let engine = SyntaxEngine::new().unwrap();
        let request = SyntaxRequest {
            generation: 1,
            request_id: 1,
            path: Some("main.rs".to_owned()),
            first_line: None,
            lines: Arc::from(["// comment".to_owned(), "let value = 1;".to_owned()]),
            theme: ThemeChoice::Dark,
        };
        let output = engine.highlight(&request).unwrap();
        let standalone = SyntaxRequest {
            generation: 2,
            request_id: 2,
            path: Some("main.rs".to_owned()),
            first_line: None,
            lines: Arc::from(["let value = 1;".to_owned()]),
            theme: ThemeChoice::Dark,
        };
        assert_eq!(output[1], engine.highlight(&standalone).unwrap()[0]);
        let HighlightedLine::Styled(first_spans) = &output[0] else {
            panic!("expected styled comment");
        };
        let HighlightedLine::Styled(second_spans) = &output[1] else {
            panic!("expected styled source");
        };
        assert!(
            first_spans
                .iter()
                .all(|span| span.byte_range.end <= "// comment".len())
        );
        assert!(
            second_spans
                .iter()
                .all(|span| span.byte_range.end <= "let value = 1;".len())
        );
    }
}
