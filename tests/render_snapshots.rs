use std::sync::Arc;

use git_ultrapager::cli::{ThemeChoice, WhitespaceChoice};
use git_ultrapager::decorations::DecorationSet;
use git_ultrapager::parser::parse_bytes;
use git_ultrapager::state::AppState;
use git_ultrapager::theme::{ColorLevel, Theme};
use git_ultrapager::ui::{FrameLines, render};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, Cell};
use ratatui::style::{Color, Modifier};
use unicode_width::UnicodeWidthStr;

const UNICODE_DIFF: &str = r#"diff --git a/src/unicode.rs b/src/unicode.rs
index 1111111..2222222 100644
--- a/src/unicode.rs
+++ b/src/unicode.rs
@@ -98,3 +98,4 @@ fn render() {
 let decomposed = "é";
-let old = "東京";
+let new = "東京 👩‍💻";
+	very_long_function("alpha", "beta", "gamma");
 return decomposed;
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StyleToken {
    foreground: Color,
    background: Color,
    modifiers: Modifier,
}

#[test]
fn dark_theme_at_narrow_width() {
    insta::assert_snapshot!(
        "dark_34x14",
        render_snapshot(34, 14, ThemeChoice::Dark, ColorLevel::TrueColor)
    );
}

#[test]
fn light_theme_at_wide_width() {
    insta::assert_snapshot!(
        "light_52x14",
        render_snapshot(52, 14, ThemeChoice::Light, ColorLevel::TrueColor)
    );
}

#[test]
fn colorless_theme_keeps_structural_modifiers() {
    insta::assert_snapshot!(
        "no_color_28x16",
        render_snapshot(28, 16, ThemeChoice::Dark, ColorLevel::None)
    );
}

fn render_snapshot(
    width: u16,
    height: u16,
    theme_choice: ThemeChoice,
    color_level: ColorLevel,
) -> String {
    let document = Arc::new(parse_bytes(UNICODE_DIFF.as_bytes()).expect("fixture parse failed"));
    let state = AppState::new(document, width, height, 4, WhitespaceChoice::Ignore, true)
        .expect("state layout failed");
    let theme = Theme::new(theme_choice, color_level);
    let decorations = DecorationSet::compute(state.document(), &theme, state.whitespace())
        .expect("decoration computation failed");
    let lines = FrameLines::load(&state).expect("frame line decode failed");
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("TestBackend の初期化に失敗した");
    terminal
        .draw(|frame| render(frame, &state, &theme, &decorations, &lines))
        .expect("cell grid の描画に失敗した");

    semantic_grid(terminal.backend().buffer())
}

/// Cell grid を ANSI ではなく、画面上の grapheme と style triple の組へ変換する。
///
/// 行末の未使用 cell は捨てるが、文字間の space は `·` として残す。全角文字や ZWJ emoji の
/// 後続 cell は grapheme 幅だけ読み飛ばし、1 grapheme を snapshot 内で一度だけ記録する。
fn semantic_grid(buffer: &Buffer) -> String {
    let mut styles = Vec::<StyleToken>::new();
    let mut rows = Vec::with_capacity(usize::from(buffer.area.height));

    for y in buffer.area.y..buffer.area.bottom() {
        let last = (buffer.area.x..buffer.area.right())
            .rev()
            .find(|x| buffer[(*x, y)].symbol() != " ");
        let Some(last) = last else {
            rows.push(format!("{y:02} <blank>"));
            continue;
        };

        let mut row = format!("{y:02} ");
        let mut active_style = None;
        let mut x = buffer.area.x;
        while x <= last {
            let cell = &buffer[(x, y)];
            let style = style_token(cell);
            let style_index = styles
                .iter()
                .position(|known| *known == style)
                .unwrap_or_else(|| {
                    styles.push(style);
                    styles.len() - 1
                });
            if active_style != Some(style_index) {
                row.push_str(&format!("<s{style_index}>"));
                active_style = Some(style_index);
            }

            let symbol = cell.symbol();
            if symbol == " " {
                row.push('·');
                x = x.saturating_add(1);
            } else {
                row.push_str(symbol);
                let width = UnicodeWidthStr::width(symbol).max(1);
                x = x.saturating_add(u16::try_from(width).unwrap_or(u16::MAX));
            }
        }
        rows.push(row);
    }

    let mut output = format!(
        "screen {}x{}\nstyles:\n",
        buffer.area.width, buffer.area.height
    );
    for (index, style) in styles.iter().enumerate() {
        output.push_str(&format!(
            "  s{index}: fg={:?} bg={:?} mod={:?}\n",
            style.foreground, style.background, style.modifiers
        ));
    }
    output.push_str("rows:\n");
    output.push_str(&rows.join("\n"));
    output
}

fn style_token(cell: &Cell) -> StyleToken {
    StyleToken {
        foreground: cell.fg,
        background: cell.bg,
        modifiers: cell.modifier,
    }
}
