//! Semantic style token と terminal color capability への変換。
//!
//! Parser と layout は RGB 値を知らず、`SemanticRole` だけを付与する。ここで返す style は
//! ratatui/direct renderer の双方が使い、colorless mode でも diff marker、太字、下線、反転から
//! 情報を復元できる。

use std::env;

use ratatui::style::{Color, Modifier, Style};

use crate::cli::ThemeChoice;

/// Terminal が表現できる color space。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorLevel {
    None,
    Ansi16,
    Ansi256,
    TrueColor,
}

/// Renderer が共有する意味的な style role。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticRole {
    Plain,
    Dim,
    CommitHeader,
    FileHeader,
    HunkHeader,
    Added,
    Removed,
    Context,
    LineNumber,
    Sticky,
    Status,
    Selected,
    SearchMatch,
    SearchCurrent,
    IntralineAdded,
    IntralineRemoved,
    Warning,
    Error,
}

/// Built-in theme と capability degradation を所有する palette。
#[derive(Clone, Debug)]
pub struct Theme {
    choice: ThemeChoice,
    level: ColorLevel,
}

impl Theme {
    /// Theme choice と color capability から palette を作る。
    #[must_use]
    pub const fn new(choice: ThemeChoice, level: ColorLevel) -> Self {
        Self { choice, level }
    }

    /// Syntax grammar選択に使うbuilt-in theme名を返す。
    #[must_use]
    pub const fn choice(&self) -> ThemeChoice {
        self.choice
    }

    /// 現在のterminal color capabilityを返す。
    #[must_use]
    pub const fn color_level(&self) -> ColorLevel {
        self.level
    }

    /// Semantic role を terminal style へ変換する。
    #[must_use]
    pub fn style(&self, role: SemanticRole) -> Style {
        let structural = structural_modifiers(role);
        if self.level == ColorLevel::None {
            return Style::default().add_modifier(structural);
        }

        let (foreground, background) = palette(self.choice, role);
        let mut style = Style::default().add_modifier(structural);
        if let Some(color) = foreground {
            style = style.fg(self.map_color(color));
        }
        if let Some(color) = background {
            style = style.bg(self.map_color(color));
        }
        style
    }

    /// Syntectが返したRGB foregroundとfont属性をterminal capabilityへ変換する。
    ///
    /// `NO_COLOR`ではsyntax由来のforegroundと属性を両方捨てる。Diff markerやselectionの属性は
    /// [`Self::style`]が別layerとして付けるため、この戻り値へ構造styleを混ぜない。
    #[must_use]
    pub fn syntax_style(
        &self,
        foreground: (u8, u8, u8),
        bold: bool,
        italic: bool,
        underline: bool,
    ) -> Style {
        if self.level == ColorLevel::None {
            return Style::default();
        }
        let mut modifiers = Modifier::empty();
        if bold {
            modifiers.insert(Modifier::BOLD);
        }
        if italic {
            modifiers.insert(Modifier::ITALIC);
        }
        if underline {
            modifiers.insert(Modifier::UNDERLINED);
        }
        Style::default()
            .fg(self.map_color(Rgb(foreground.0, foreground.1, foreground.2)))
            .add_modifier(modifiers)
    }

    fn map_color(&self, rgb: Rgb) -> Color {
        match self.level {
            ColorLevel::None => Color::Reset,
            ColorLevel::TrueColor => Color::Rgb(rgb.0, rgb.1, rgb.2),
            ColorLevel::Ansi256 => Color::Indexed(rgb_to_ansi256(rgb)),
            ColorLevel::Ansi16 => rgb_to_ansi16(rgb),
        }
    }
}

/// Environment と CLI の no-color 指定から color capability を求める。
#[must_use]
pub fn detect_color_level(no_color_flag: bool) -> ColorLevel {
    classify_color_level(
        no_color_flag,
        env::var_os("NO_COLOR").is_some(),
        env::var("COLORTERM").ok().as_deref(),
        env::var("TERM").ok().as_deref(),
    )
}

fn classify_color_level(
    no_color_flag: bool,
    no_color_environment: bool,
    color_term: Option<&str>,
    term: Option<&str>,
) -> ColorLevel {
    if no_color_flag || no_color_environment {
        return ColorLevel::None;
    }

    let color_term = color_term.unwrap_or_default().to_ascii_lowercase();
    let term = term.unwrap_or_default().to_ascii_lowercase();
    if color_term.contains("truecolor")
        || color_term.contains("24bit")
        || term.contains("truecolor")
        || term.contains("24bit")
        || term.ends_with("-direct")
    {
        return ColorLevel::TrueColor;
    }

    if color_term.contains("256color") || term.contains("256color") {
        ColorLevel::Ansi256
    } else {
        ColorLevel::Ansi16
    }
}

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);

fn palette(choice: ThemeChoice, role: SemanticRole) -> (Option<Rgb>, Option<Rgb>) {
    let dark = choice == ThemeChoice::Dark;
    match role {
        SemanticRole::Plain | SemanticRole::Context => (None, None),
        SemanticRole::Dim | SemanticRole::LineNumber => (
            Some(if dark {
                Rgb(115, 125, 145)
            } else {
                Rgb(92, 99, 112)
            }),
            None,
        ),
        SemanticRole::CommitHeader => (
            Some(if dark {
                Rgb(199, 166, 255)
            } else {
                Rgb(103, 58, 183)
            }),
            None,
        ),
        SemanticRole::FileHeader => (
            Some(if dark {
                Rgb(130, 210, 255)
            } else {
                Rgb(0, 92, 153)
            }),
            None,
        ),
        SemanticRole::HunkHeader => (
            Some(if dark {
                Rgb(132, 220, 198)
            } else {
                Rgb(0, 105, 92)
            }),
            Some(if dark {
                Rgb(23, 50, 55)
            } else {
                Rgb(221, 245, 241)
            }),
        ),
        SemanticRole::Added => (
            Some(if dark {
                Rgb(166, 227, 161)
            } else {
                Rgb(19, 102, 54)
            }),
            Some(if dark {
                Rgb(24, 55, 35)
            } else {
                Rgb(225, 246, 232)
            }),
        ),
        SemanticRole::Removed => (
            Some(if dark {
                Rgb(243, 139, 168)
            } else {
                Rgb(161, 37, 63)
            }),
            Some(if dark {
                Rgb(65, 31, 42)
            } else {
                Rgb(253, 231, 236)
            }),
        ),
        SemanticRole::Sticky => (
            Some(if dark {
                Rgb(235, 238, 245)
            } else {
                Rgb(34, 39, 48)
            }),
            Some(if dark {
                Rgb(55, 62, 78)
            } else {
                Rgb(222, 226, 235)
            }),
        ),
        SemanticRole::Status => (
            Some(if dark {
                Rgb(210, 215, 228)
            } else {
                Rgb(51, 58, 71)
            }),
            Some(if dark {
                Rgb(38, 43, 55)
            } else {
                Rgb(235, 237, 242)
            }),
        ),
        SemanticRole::Selected => (
            Some(if dark {
                Rgb(20, 23, 31)
            } else {
                Rgb(255, 255, 255)
            }),
            Some(if dark {
                Rgb(137, 180, 250)
            } else {
                Rgb(48, 94, 163)
            }),
        ),
        SemanticRole::SearchMatch => (
            None,
            Some(if dark {
                Rgb(90, 76, 20)
            } else {
                Rgb(255, 237, 153)
            }),
        ),
        SemanticRole::SearchCurrent => (
            Some(if dark {
                Rgb(20, 23, 31)
            } else {
                Rgb(255, 255, 255)
            }),
            Some(if dark {
                Rgb(249, 226, 175)
            } else {
                Rgb(158, 104, 0)
            }),
        ),
        SemanticRole::IntralineAdded => (
            None,
            Some(if dark {
                Rgb(45, 95, 59)
            } else {
                Rgb(181, 230, 195)
            }),
        ),
        SemanticRole::IntralineRemoved => (
            None,
            Some(if dark {
                Rgb(105, 47, 61)
            } else {
                Rgb(246, 185, 197)
            }),
        ),
        SemanticRole::Warning => (
            Some(if dark {
                Rgb(249, 226, 175)
            } else {
                Rgb(133, 88, 0)
            }),
            None,
        ),
        SemanticRole::Error => (
            Some(if dark {
                Rgb(255, 128, 150)
            } else {
                Rgb(176, 0, 32)
            }),
            None,
        ),
    }
}

fn structural_modifiers(role: SemanticRole) -> Modifier {
    match role {
        SemanticRole::CommitHeader
        | SemanticRole::FileHeader
        | SemanticRole::HunkHeader
        | SemanticRole::Sticky => Modifier::BOLD,
        SemanticRole::Dim | SemanticRole::LineNumber => Modifier::DIM,
        // Overlay selectionはcolorless modeでも位置を失わないようreverse videoを契約に含める。
        SemanticRole::Selected => Modifier::BOLD | Modifier::REVERSED,
        SemanticRole::SearchCurrent => Modifier::BOLD,
        SemanticRole::SearchMatch => Modifier::UNDERLINED,
        SemanticRole::Warning => Modifier::ITALIC,
        SemanticRole::Error => Modifier::BOLD,
        _ => Modifier::empty(),
    }
}

fn rgb_to_ansi256(rgb: Rgb) -> u8 {
    fn component(value: u8) -> u8 {
        ((u16::from(value) * 5 + 127) / 255) as u8
    }

    16 + 36 * component(rgb.0) + 6 * component(rgb.1) + component(rgb.2)
}

fn rgb_to_ansi16(rgb: Rgb) -> Color {
    let bright = u16::from(rgb.0) + u16::from(rgb.1) + u16::from(rgb.2) >= 384;
    let red = rgb.0 >= 128;
    let green = rgb.1 >= 128;
    let blue = rgb.2 >= 128;
    match (bright, red, green, blue) {
        (false, false, false, false) => Color::Black,
        (false, true, false, false) => Color::Red,
        (false, false, true, false) => Color::Green,
        (false, true, true, false) => Color::Yellow,
        (false, false, false, true) => Color::Blue,
        (false, true, false, true) => Color::Magenta,
        (false, false, true, true) => Color::Cyan,
        (false, true, true, true) => Color::Gray,
        (true, false, false, false) => Color::DarkGray,
        (true, true, false, false) => Color::LightRed,
        (true, false, true, false) => Color::LightGreen,
        (true, true, true, false) => Color::LightYellow,
        (true, false, false, true) => Color::LightBlue,
        (true, true, false, true) => Color::LightMagenta,
        (true, false, true, true) => Color::LightCyan,
        (true, true, true, true) => Color::White,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorless_theme_keeps_structural_modifiers() {
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let style = theme.style(SemanticRole::SearchMatch);
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(style.fg, None);
        assert_eq!(style.bg, None);
    }

    #[test]
    fn true_color_keeps_palette_rgb() {
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::TrueColor);
        assert!(matches!(
            theme.style(SemanticRole::Added).fg,
            Some(Color::Rgb(..))
        ));
    }

    #[test]
    fn colorless_selection_uses_reverse_video() {
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        assert!(
            theme
                .style(SemanticRole::Selected)
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn capability_classification_uses_both_terminal_variables() {
        assert_eq!(
            classify_color_level(false, false, None, Some("xterm-direct")),
            ColorLevel::TrueColor
        );
        assert_eq!(
            classify_color_level(false, false, Some("256color"), None),
            ColorLevel::Ansi256
        );
        assert_eq!(
            classify_color_level(false, false, None, None),
            ColorLevel::Ansi16
        );
        assert_eq!(
            classify_color_level(false, true, Some("truecolor"), Some("xterm-direct")),
            ColorLevel::None
        );
    }

    #[test]
    fn syntax_rgb_respects_terminal_capability_and_no_color() {
        let true_color = Theme::new(ThemeChoice::Dark, ColorLevel::TrueColor).syntax_style(
            (12, 34, 56),
            true,
            false,
            true,
        );
        assert_eq!(true_color.fg, Some(Color::Rgb(12, 34, 56)));
        assert!(true_color.add_modifier.contains(Modifier::BOLD));
        assert!(true_color.add_modifier.contains(Modifier::UNDERLINED));

        let colorless = Theme::new(ThemeChoice::Dark, ColorLevel::None).syntax_style(
            (12, 34, 56),
            true,
            true,
            true,
        );
        assert_eq!(colorless, Style::default());
    }
}
