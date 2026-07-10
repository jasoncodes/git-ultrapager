//! CLI option の schema と validation を所有する。
//!
//! positional file、less option、repository path は受け付けない。ここで得た値は起動時の初期値で、
//! interactive 中の `W` toggle は process 外へ永続化しない。

use clap::{Parser, ValueEnum};

/// `git-ultrapager` の command-line contract。
#[derive(Clone, Debug, Parser, PartialEq, Eq)]
#[command(version, about, disable_help_subcommand = true)]
pub struct Cli {
    /// Built-in color theme の初期値。
    #[arg(
        long,
        value_enum,
        default_value_t = ThemeChoice::Dark,
        help = "Select the built-in color theme"
    )]
    pub theme: ThemeChoice,

    /// Syntax とdiffの色だけを無効にし、構造を表す属性は残す。
    #[arg(
        long,
        help = "Disable syntax and diff colors while retaining structural attributes"
    )]
    pub no_color: bool,

    /// Interactive pager を開く条件。
    #[arg(
        long,
        value_enum,
        default_value_t = PagingChoice::Auto,
        help = "Control whether the interactive pager opens"
    )]
    pub paging: PagingChoice,

    /// Source text のtab stop幅。parserが生成したmetadataには適用しない。
    #[arg(
        long,
        default_value_t = 4,
        value_parser = clap::value_parser!(u8).range(1..=16),
        help = "Set source-code tab stops"
    )]
    pub tab_width: u8,

    /// Intraline比較でASCII space/tabを無視するかの初期値。
    #[arg(
        long,
        value_enum,
        default_value_t = WhitespaceChoice::Ignore,
        help = "Choose whether intraline comparison ignores horizontal whitespace"
    )]
    pub whitespace: WhitespaceChoice,
}

/// Built-in theme selection。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
}

/// Pager activation policy。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum PagingChoice {
    #[default]
    Auto,
    Always,
    Never,
}

/// Intraline whitespace comparison policy。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum WhitespaceChoice {
    #[default]
    Ignore,
    Show,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_pager_contract() {
        let cli = Cli::try_parse_from(["git-ultrapager"]).unwrap();
        assert_eq!(cli.theme, ThemeChoice::Dark);
        assert!(!cli.no_color);
        assert_eq!(cli.paging, PagingChoice::Auto);
        assert_eq!(cli.tab_width, 4);
        assert_eq!(cli.whitespace, WhitespaceChoice::Ignore);
    }

    #[test]
    fn parses_every_public_choice() {
        let cli = Cli::try_parse_from([
            "git-ultrapager",
            "--theme",
            "light",
            "--no-color",
            "--paging",
            "always",
            "--tab-width",
            "16",
            "--whitespace",
            "show",
        ])
        .unwrap();
        assert_eq!(cli.theme, ThemeChoice::Light);
        assert!(cli.no_color);
        assert_eq!(cli.paging, PagingChoice::Always);
        assert_eq!(cli.tab_width, 16);
        assert_eq!(cli.whitespace, WhitespaceChoice::Show);
    }

    #[test]
    fn rejects_files_and_out_of_range_tab_widths() {
        assert!(Cli::try_parse_from(["git-ultrapager", "patch.diff"]).is_err());
        assert!(Cli::try_parse_from(["git-ultrapager", "--tab-width", "0"]).is_err());
        assert!(Cli::try_parse_from(["git-ultrapager", "--tab-width", "17"]).is_err());
    }
}
