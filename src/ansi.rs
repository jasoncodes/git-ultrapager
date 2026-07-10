//! stdin に含まれる terminal control sequence と表示不能 byte の境界を所有する。
//!
//! [`AnsiStripper`] は chunk 境界をまたぐ CSI / OSC / DCS を実行可能な形で後段へ渡さない。
//! parser は除去後の byte 列を改行単位に分け、[`escape_interactive_bytes`] で UTF-8 と
//! control byte を表示用 text へ変換する。non-TTY passthrough はこの module を通さず、raw byte
//! を維持する。

use std::fmt::Write as _;

/// ANSI sequence 除去器が次の chunk まで保持する状態。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
    Dcs,
    DcsEscape,
    String,
    StringEscape,
}

/// ANSI / ECMA-48 の terminal control sequence を byte stream から逐次除去する。
///
/// `push` は sequence の途中で chunk が終わっても状態を保持する。未完の sequence は `finish`
/// で破棄されるため、入力末尾の `ESC` や OSC payload が terminal へ漏れない。
#[derive(Clone, Debug, Default)]
pub struct AnsiStripper {
    state: State,
    /// raw C1 control と UTF-8 continuation byte を区別するために保持する残り byte 数。
    utf8_continuations: u8,
}

impl AnsiStripper {
    /// control sequence を除いた byte を `output` の末尾へ追加する。
    ///
    /// LF、CR、tab を含む通常 byte の可視化は行わない。呼び出し側が改行を確定してから
    /// [`escape_interactive_bytes`] を適用する契約である。
    pub fn push(&mut self, input: &[u8], output: &mut Vec<u8>) {
        for &byte in input {
            self.consume(byte, output);
        }
    }

    /// 未完の terminal sequence を破棄して初期状態へ戻す。
    pub fn finish(&mut self) {
        self.state = State::Ground;
        self.utf8_continuations = 0;
    }

    fn consume(&mut self, byte: u8, output: &mut Vec<u8>) {
        // 0x90 / 0x9B / 0x9D は8-bit C1 control としても、valid UTF-8 の continuation byte
        // としても現れる。lead byte を出力済みなら sequence 判定より UTF-8 の保持を優先する。
        if matches!(
            self.state,
            State::Ground | State::Osc | State::Dcs | State::String
        ) && self.utf8_continuations > 0
        {
            if (0x80..=0xbf).contains(&byte) {
                if self.state == State::Ground {
                    output.push(byte);
                }
                self.utf8_continuations = self.utf8_continuations.saturating_sub(1);
                return;
            }
            self.utf8_continuations = 0;
        }

        self.state = match self.state {
            State::Ground => match byte {
                0x1b => State::Escape,
                0x9b => State::Csi,
                0x9d => State::Osc,
                0x90 => State::Dcs,
                0x98 | 0x9e | 0x9f => State::String,
                0x9c => State::Ground,
                _ => {
                    output.push(byte);
                    self.utf8_continuations = utf8_continuations(byte);
                    State::Ground
                }
            },
            State::Escape => match byte {
                b'[' => State::Csi,
                b']' => State::Osc,
                b'P' => State::Dcs,
                b'X' | b'^' | b'_' => State::String,
                // ESC の intermediate byte は終端 byte が来るまで除去する。
                0x20..=0x2f => State::Escape,
                _ => State::Ground,
            },
            State::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    State::Ground
                } else {
                    State::Csi
                }
            }
            State::Osc => match byte {
                0x07 | 0x9c => State::Ground,
                0x1b => State::OscEscape,
                _ => {
                    self.utf8_continuations = utf8_continuations(byte);
                    State::Osc
                }
            },
            State::OscEscape => {
                if byte == b'\\' {
                    State::Ground
                } else if byte == 0x1b {
                    State::OscEscape
                } else {
                    State::Osc
                }
            }
            State::Dcs => match byte {
                0x9c => State::Ground,
                0x1b => State::DcsEscape,
                _ => {
                    self.utf8_continuations = utf8_continuations(byte);
                    State::Dcs
                }
            },
            State::DcsEscape => {
                if byte == b'\\' {
                    State::Ground
                } else if byte == 0x1b {
                    State::DcsEscape
                } else {
                    State::Dcs
                }
            }
            State::String => match byte {
                0x9c => State::Ground,
                0x1b => State::StringEscape,
                _ => {
                    self.utf8_continuations = utf8_continuations(byte);
                    State::String
                }
            },
            State::StringEscape => {
                if byte == b'\\' {
                    State::Ground
                } else if byte == 0x1b {
                    State::StringEscape
                } else {
                    State::String
                }
            }
        };
    }
}

const fn utf8_continuations(byte: u8) -> u8 {
    match byte {
        0xc2..=0xdf => 1,
        0xe0..=0xef => 2,
        0xf0..=0xf4 => 3,
        _ => 0,
    }
}

/// 1つの byte slice から ANSI sequence を除去する。
///
/// streaming 入力には、chunk ごとに状態を保持できる [`AnsiStripper`] を使う。
#[must_use]
pub fn strip_ansi(input: &[u8]) -> Vec<u8> {
    let mut stripper = AnsiStripper::default();
    let mut output = Vec::with_capacity(input.len());
    stripper.push(input, &mut output);
    stripper.finish();
    output
}

/// untrusted byte 列を terminal へ安全に描画できる UTF-8 text へ変換する。
///
/// valid UTF-8 は維持し、invalid UTF-8 と C0/C1 control は元 byte ごとの `\\xNN` にする。
/// LF と tab は parser の行分割と tab-stop 展開が所有するため、そのまま残す。ESC は通常この関数
/// より前に除去されるが、単独で渡された場合も `\\x1B` となり実行されない。
#[must_use]
pub fn escape_interactive_bytes(input: &[u8]) -> String {
    escape_bytes(input, true)
}

/// path を1行の header / overlay に表示できる UTF-8 text へ変換する。
///
/// path 内では LF と tab も layout を変更してはならないため、通常の control byte と同じ
/// `\\xNN` 表記にする。
#[must_use]
pub fn escape_path_bytes(input: &[u8]) -> String {
    escape_bytes(input, false)
}

fn escape_bytes(mut input: &[u8], preserve_layout_controls: bool) -> String {
    let mut output = String::with_capacity(input.len());

    while !input.is_empty() {
        match std::str::from_utf8(input) {
            Ok(valid) => {
                push_valid_text(&mut output, valid, preserve_layout_controls);
                break;
            }
            Err(error) => {
                let valid_len = error.valid_up_to();
                if valid_len > 0 {
                    // `valid_up_to` は UTF-8 boundary を保証する。
                    if let Ok(valid) = std::str::from_utf8(&input[..valid_len]) {
                        push_valid_text(&mut output, valid, preserve_layout_controls);
                    }
                }

                let invalid_len = error
                    .error_len()
                    .unwrap_or_else(|| input.len().saturating_sub(valid_len));
                let invalid_end = valid_len.saturating_add(invalid_len).min(input.len());
                for &byte in &input[valid_len..invalid_end] {
                    push_hex_escape(&mut output, byte);
                }
                input = &input[invalid_end..];
            }
        }
    }

    output
}

fn push_valid_text(output: &mut String, valid: &str, preserve_layout_controls: bool) {
    for character in valid.chars() {
        let codepoint = u32::from(character);
        let allowed_layout_control = preserve_layout_controls && matches!(character, '\n' | '\t');
        if allowed_layout_control
            || (!character.is_control() && !(0x7f..=0x9f).contains(&codepoint))
        {
            output.push(character);
        } else {
            let mut bytes = [0_u8; 4];
            for &byte in character.encode_utf8(&mut bytes).as_bytes() {
                push_hex_escape(output, byte);
            }
        }
    }
}

fn push_hex_escape(output: &mut String, byte: u8) {
    // String への write は allocation failure 以外で失敗しないため、Result は捨ててよい。
    let _ = write!(output, "\\x{byte:02X}");
}
