//! stdin に含まれる terminal control sequence と表示不能 byte の境界を所有する。
//!
//! [`AnsiStripper`] は chunk 境界をまたぐ CSI / OSC / DCS を実行可能な形で後段へ渡さない。
//! parser は除去後の byte 列を改行単位に分け、[`escape_interactive_bytes`] で UTF-8 と
//! control byte を表示用 text へ変換する。non-TTY passthrough は除去後のbyteをescapeせずに書き、
//! ANSI sequence以外のinvalid UTF-8と改行状態を維持する。

use thiserror::Error;

/// Chunk末尾で次回入力まで保持し、次の`push_indexed`または`finish_indexed`で追加出力し得る最大byte数。
///
/// 4-byte UTF-8 scalarは3byte受信した時点で未完となり、4byte目が来るまで先頭3byteを保持する。
/// Parserは現在chunk長へこの値を足してcallback出力bufferを先に確保し、callback内の再確保を避ける。
pub(crate) const MAX_DEFERRED_OUTPUT_BYTES: usize = 3;

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
    /// Scalarが完成するまで最大4byteを保留する。途中でinvalidと分かった時はcontinuationを
    /// C1として再評価するため、terminal controlになり得るbyteを先行出力しない。
    utf8_pending: Option<Utf8Pending>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Utf8Tail {
    remaining: u8,
    next_min: u8,
    next_max: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BufferedByte {
    offset: u64,
    byte: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Utf8Pending {
    origin_state: State,
    bytes: [BufferedByte; 4],
    len: u8,
    tail: Utf8Tail,
}

impl Utf8Pending {
    fn after_lead(origin_state: State, offset: u64, byte: u8) -> Option<Self> {
        let tail = Utf8Tail::after_lead(byte)?;
        let mut bytes = [BufferedByte::default(); 4];
        bytes[0] = BufferedByte { offset, byte };
        Some(Self {
            origin_state,
            bytes,
            len: 1,
            tail,
        })
    }

    fn accepts(self, byte: u8) -> bool {
        (self.tail.next_min..=self.tail.next_max).contains(&byte)
    }

    fn push(&mut self, offset: u64, byte: u8) {
        self.bytes[usize::from(self.len)] = BufferedByte { offset, byte };
        self.len += 1;
        self.tail.remaining -= 1;
        if self.tail.remaining > 0 {
            self.tail.next_min = 0x80;
            self.tail.next_max = 0xbf;
        }
    }

    fn is_complete(self) -> bool {
        self.tail.remaining == 0
    }

    fn bytes(self) -> impl Iterator<Item = BufferedByte> {
        self.bytes.into_iter().take(usize::from(self.len))
    }
}

impl Utf8Tail {
    const fn after_lead(byte: u8) -> Option<Self> {
        match byte {
            0xc2..=0xdf => Some(Self::new(1, 0x80, 0xbf)),
            0xe0 => Some(Self::new(2, 0xa0, 0xbf)),
            0xe1..=0xec | 0xee..=0xef => Some(Self::new(2, 0x80, 0xbf)),
            0xed => Some(Self::new(2, 0x80, 0x9f)),
            0xf0 => Some(Self::new(3, 0x90, 0xbf)),
            0xf1..=0xf3 => Some(Self::new(3, 0x80, 0xbf)),
            0xf4 => Some(Self::new(3, 0x80, 0x8f)),
            _ => None,
        }
    }

    const fn new(remaining: u8, next_min: u8, next_max: u8) -> Self {
        Self {
            remaining,
            next_min,
            next_max,
        }
    }
}

/// Indexed sanitization のraw absolute offsetが`u64`を超える入力。
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("raw input offset overflow")]
pub struct RawOffsetOverflow;

/// Escape後の正確な長さが`usize`を超えるか、出力bufferを確保できなかった時のfailure。
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum EscapeError {
    #[error("escaped length overflow for {input_len} input bytes")]
    LengthOverflow { input_len: usize },
    #[error("could not allocate space for exactly {requested} escaped bytes")]
    Allocation { requested: usize },
    #[error("UTF-8 validation returned an invalid prefix boundary")]
    InvalidUtf8Boundary,
}

impl AnsiStripper {
    /// control sequence を除いた byte を `output` の末尾へ追加する。
    ///
    /// LF、CR、tab を含む通常 byte の可視化は行わない。呼び出し側が改行を確定してから
    /// [`escape_interactive_bytes`] を適用する。`push_indexed`とは同じinstance上で混在させない。
    pub fn push(&mut self, input: &[u8], output: &mut Vec<u8>) {
        let mut emit = |_: u64, byte| output.push(byte);
        for &byte in input {
            self.consume_indexed(0, byte, &mut emit);
        }
    }

    /// control sequenceを除いた各byteと、そのbyteが由来するraw absolute offsetを通知する。
    ///
    /// Callbackはraw順に最大1回ずつ呼ばれる。ANSI sequence内のbyteには呼ばれず、LFもOSC/DCS等の
    /// string内なら通知されない。これによりparserは「表示上のlogical line」を閉じたraw rangeを、
    /// sanitized textのcopyを保持せず記録できる。Offset範囲はstateを進める前に検証するため、overflow
    /// error後も同じstripperを別入力へ再利用できる。`push`とは同じinstance上で混在させない。
    pub fn push_indexed<F>(
        &mut self,
        raw_base: u64,
        input: &[u8],
        mut emit: F,
    ) -> Result<(), RawOffsetOverflow>
    where
        F: FnMut(u64, u8),
    {
        if let Some(last) = input.len().checked_sub(1) {
            let last = u64::try_from(last).map_err(|_| RawOffsetOverflow)?;
            let _ = raw_base.checked_add(last).ok_or(RawOffsetOverflow)?;
        }
        for (relative, &byte) in input.iter().enumerate() {
            let relative = u64::try_from(relative).map_err(|_| RawOffsetOverflow)?;
            self.consume_indexed(raw_base + relative, byte, &mut emit);
        }
        Ok(())
    }

    /// 未完のUTF-8 textとterminal sequenceを破棄して初期状態へ戻す。
    ///
    /// Text byteを保持するstreaming callerは[`Self::finish_into`]、indexed callerは
    /// [`Self::finish_indexed`]を使う。
    pub fn finish(&mut self) {
        self.finish_indexed(|_, _| {});
    }

    /// EOFで未完だったUTF-8 scalarをraw textとして再評価し、安全なbyteを`output`へ追加する。
    ///
    /// Replay中にCSI/OSC/DCS開始と判定したbyte以降と、EOF時点で未完のterminal sequenceは破棄する。
    pub fn finish_into(&mut self, output: &mut Vec<u8>) {
        self.finish_indexed(|_, byte| output.push(byte));
    }

    /// [`Self::finish_into`]と同じEOF処理を行い、保持していたraw offsetとbyteを通知する。
    pub fn finish_indexed<F>(&mut self, mut emit: F)
    where
        F: FnMut(u64, u8),
    {
        if let Some(pending) = self.utf8_pending.take() {
            self.replay_invalid_pending(pending, &mut emit);
        }
        self.state = State::Ground;
        self.utf8_pending = None;
    }

    fn consume_indexed<F>(&mut self, offset: u64, byte: u8, emit: &mut F)
    where
        F: FnMut(u64, u8),
    {
        if is_string_payload_state(self.state)
            && let Some(mut pending) = self.utf8_pending.take()
        {
            if pending.accepts(byte) {
                pending.push(offset, byte);
                if pending.is_complete() && pending.origin_state == State::Ground {
                    for buffered in pending.bytes() {
                        emit(buffered.offset, buffered.byte);
                    }
                } else if !pending.is_complete() {
                    self.utf8_pending = Some(pending);
                }
                return;
            }
            self.replay_invalid_pending(pending, emit);
        }

        self.consume_raw(offset, byte, emit);
    }

    fn replay_invalid_pending<F>(&mut self, pending: Utf8Pending, emit: &mut F)
    where
        F: FnMut(u64, u8),
    {
        self.state = pending.origin_state;
        let mut bytes = pending.bytes();
        if let Some(lead) = bytes.next()
            && self.state == State::Ground
        {
            emit(lead.offset, lead.byte);
        }
        for buffered in bytes {
            self.consume_raw(buffered.offset, buffered.byte, emit);
        }
    }

    fn consume_raw<F>(&mut self, offset: u64, byte: u8, emit: &mut F)
    where
        F: FnMut(u64, u8),
    {
        self.state = match self.state {
            State::Ground => match byte {
                0x1b => State::Escape,
                0x9b => State::Csi,
                0x9d => State::Osc,
                0x90 => State::Dcs,
                0x98 | 0x9e | 0x9f => State::String,
                0x9c => State::Ground,
                _ => {
                    if let Some(pending) = Utf8Pending::after_lead(State::Ground, offset, byte) {
                        self.utf8_pending = Some(pending);
                    } else {
                        emit(offset, byte);
                    }
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
                    self.utf8_pending = Utf8Pending::after_lead(State::Osc, offset, byte);
                    State::Osc
                }
            },
            State::OscEscape => {
                if byte == b'\\' {
                    State::Ground
                } else if byte == 0x1b {
                    State::OscEscape
                } else {
                    self.state = State::Osc;
                    self.consume_raw(offset, byte, emit);
                    return;
                }
            }
            State::Dcs => match byte {
                0x9c => State::Ground,
                0x1b => State::DcsEscape,
                _ => {
                    self.utf8_pending = Utf8Pending::after_lead(State::Dcs, offset, byte);
                    State::Dcs
                }
            },
            State::DcsEscape => {
                if byte == b'\\' {
                    State::Ground
                } else if byte == 0x1b {
                    State::DcsEscape
                } else {
                    self.state = State::Dcs;
                    self.consume_raw(offset, byte, emit);
                    return;
                }
            }
            State::String => match byte {
                0x9c => State::Ground,
                0x1b => State::StringEscape,
                _ => {
                    self.utf8_pending = Utf8Pending::after_lead(State::String, offset, byte);
                    State::String
                }
            },
            State::StringEscape => {
                if byte == b'\\' {
                    State::Ground
                } else if byte == 0x1b {
                    State::StringEscape
                } else {
                    self.state = State::String;
                    self.consume_raw(offset, byte, emit);
                    return;
                }
            }
        };
    }
}

const fn is_string_payload_state(state: State) -> bool {
    matches!(
        state,
        State::Ground | State::Osc | State::Dcs | State::String
    )
}

/// 1つの byte slice から ANSI sequence を除去する。
///
/// streaming 入力には、chunk ごとに状態を保持できる [`AnsiStripper`] を使う。
#[must_use]
pub fn strip_ansi(input: &[u8]) -> Vec<u8> {
    let mut stripper = AnsiStripper::default();
    let mut output = Vec::with_capacity(input.len());
    stripper.push(input, &mut output);
    stripper.finish_into(&mut output);
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

/// Untrusted byte列を既存の`String`へescapeし、内容を置き換える。
///
/// 出力の正確なbyte数を`output`の変更前に計算し、その長さだけをreserveする。長さoverflowまたは
/// allocation failureでは既存の内容を維持する。成功後の長さは予約済みなので変換途中にallocationは
/// 発生しない。
pub fn escape_interactive_bytes_into(input: &[u8], output: &mut String) -> Result<(), EscapeError> {
    escape_interactive_bytes_into_counted(input, output).map(|_| ())
}

/// [`escape_interactive_bytes_into`]と同じ変換を行い、正確な出力byte数を返す。
///
/// Logical-line decoderはこの値を固定長recordの`display_len`と照合する。公開API側と同じ変換関数を
/// 通すため、invalid UTF-8やUTF-8 encoded controlで長さの数え方が分岐しない。
pub(crate) fn escape_interactive_bytes_into_counted(
    input: &[u8],
    output: &mut String,
) -> Result<usize, EscapeError> {
    escape_bytes_into(input, true, output)
}

/// Chunk境界をまたぐUTF-8 scalarを最大4 byteだけ保留し、interactive向けescapeを逐次生成する。
///
/// [`escape_interactive_bytes_into_counted`]と同じbyte列を返すが、logical line全体を入力・出力の
/// どちらにも保持しない。`push_into`は`output`へ追記し、`finish_into`はEOF時に未完だったUTF-8
/// prefixをbyte単位の`\\xNN`へ変換する。Callerはlogical lineごとに1 instanceを使い、完了後に
/// [`Self::reset`]して次のlineへ移る。
#[derive(Clone, Debug, Default)]
pub(crate) struct StreamingInteractiveEscaper {
    pending: Option<StreamingUtf8Pending>,
}

#[derive(Clone, Copy, Debug)]
struct StreamingUtf8Pending {
    bytes: [u8; 4],
    len: u8,
    tail: Utf8Tail,
}

impl StreamingUtf8Pending {
    fn after_lead(byte: u8) -> Option<Self> {
        let tail = Utf8Tail::after_lead(byte)?;
        let mut bytes = [0_u8; 4];
        bytes[0] = byte;
        Some(Self {
            bytes,
            len: 1,
            tail,
        })
    }

    fn accepts(self, byte: u8) -> bool {
        (self.tail.next_min..=self.tail.next_max).contains(&byte)
    }

    fn push(&mut self, byte: u8) {
        self.bytes[usize::from(self.len)] = byte;
        self.len += 1;
        self.tail.remaining -= 1;
        if self.tail.remaining > 0 {
            self.tail.next_min = 0x80;
            self.tail.next_max = 0xbf;
        }
    }

    const fn is_complete(self) -> bool {
        self.tail.remaining == 0
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl StreamingInteractiveEscaper {
    /// Chunkを変換し、`output`末尾へ追加したbyte数を返す。
    ///
    /// 1入力byteが最大4 ASCII byteへ展開されるため、その上限だけをfallibleに予約する。UTF-8 scalar
    /// の途中でchunkが終わった場合は最大3 byteをinstance内に残し、この呼び出しの出力には含めない。
    pub(crate) fn push_into(
        &mut self,
        input: &[u8],
        output: &mut String,
    ) -> Result<usize, EscapeError> {
        let pending_len = self
            .pending
            .as_ref()
            .map_or(0, |pending| usize::from(pending.len));
        let worst_input =
            input
                .len()
                .checked_add(pending_len)
                .ok_or(EscapeError::LengthOverflow {
                    input_len: input.len(),
                })?;
        let worst_added = worst_input
            .checked_mul(4)
            .ok_or(EscapeError::LengthOverflow {
                input_len: input.len(),
            })?;
        let requested =
            output
                .len()
                .checked_add(worst_added)
                .ok_or(EscapeError::LengthOverflow {
                    input_len: input.len(),
                })?;
        output
            .try_reserve(worst_added)
            .map_err(|_| EscapeError::Allocation { requested })?;

        let before = output.len();
        for &byte in input {
            self.push_byte(byte, output)?;
        }
        Ok(output.len() - before)
    }

    /// 未完のUTF-8 prefixをescapeして`output`へ追加し、追加byte数を返す。
    pub(crate) fn finish_into(&mut self, output: &mut String) -> Result<usize, EscapeError> {
        let Some(pending) = self.pending.take() else {
            return Ok(0);
        };
        let added = usize::from(pending.len)
            .checked_mul(4)
            .ok_or(EscapeError::LengthOverflow {
                input_len: usize::from(pending.len),
            })?;
        let requested = output
            .len()
            .checked_add(added)
            .ok_or(EscapeError::LengthOverflow {
                input_len: usize::from(pending.len),
            })?;
        output
            .try_reserve(added)
            .map_err(|_| EscapeError::Allocation { requested })?;
        for &byte in pending.as_bytes() {
            push_hex_escape(output, byte);
        }
        Ok(added)
    }

    /// 保留scalarを破棄し、別logical lineの先頭stateへ戻す。
    pub(crate) fn reset(&mut self) {
        self.pending = None;
    }

    fn push_byte(&mut self, byte: u8, output: &mut String) -> Result<(), EscapeError> {
        if let Some(mut pending) = self.pending.take() {
            if pending.accepts(byte) {
                pending.push(byte);
                if pending.is_complete() {
                    let valid = std::str::from_utf8(pending.as_bytes())
                        .map_err(|_| EscapeError::InvalidUtf8Boundary)?;
                    push_valid_text(output, valid, true);
                } else {
                    self.pending = Some(pending);
                }
                return Ok(());
            }
            for &invalid in pending.as_bytes() {
                push_hex_escape(output, invalid);
            }
        }

        if byte.is_ascii() {
            let text = [byte];
            let valid = std::str::from_utf8(&text).map_err(|_| EscapeError::InvalidUtf8Boundary)?;
            push_valid_text(output, valid, true);
        } else if let Some(pending) = StreamingUtf8Pending::after_lead(byte) {
            self.pending = Some(pending);
        } else {
            push_hex_escape(output, byte);
        }
        Ok(())
    }
}

/// path を1行の header / overlay に表示できる UTF-8 text へ変換する。
///
/// path 内では LF と tab も layout を変更してはならないため、通常の control byte と同じ
/// `\\xNN` 表記にする。
#[must_use]
pub fn escape_path_bytes(input: &[u8]) -> String {
    escape_bytes(input, false)
}

/// Path用escapeの結果、または指定byte数を越えたため出力を確保しなかった結果。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LimitedPathEscape {
    Escaped(String),
    LimitExceeded { escaped_len: usize },
}

/// Pathをescapeした正確なUTF-8 byte数が`limit`以下の場合だけ出力を確保する。
///
/// `LimitExceeded`では長さ検査だけを行い、escaped text用のbufferを作らない。入力欄の上限を
/// 検査するcallerは、既存textへ挿入する前にこの関数を呼ぶことで拒否時の内容を維持できる。
pub(crate) fn escape_path_bytes_with_limit(
    input: &[u8],
    limit: usize,
) -> Result<LimitedPathEscape, EscapeError> {
    let escaped_len = escaped_bytes_len(input, false)?;
    if escaped_len > limit {
        return Ok(LimitedPathEscape::LimitExceeded { escaped_len });
    }

    let mut output = String::new();
    reserve_escape_capacity(&mut output, escaped_len)?;
    push_escaped_bytes(input, false, &mut output);
    debug_assert_eq!(output.len(), escaped_len);
    Ok(LimitedPathEscape::Escaped(output))
}

fn escape_bytes(input: &[u8], preserve_layout_controls: bool) -> String {
    let mut output = String::new();
    match escape_bytes_into(input, preserve_layout_controls, &mut output) {
        Ok(_) => output,
        Err(error) => panic!("failed to allocate escaped byte string: {error}"),
    }
}

fn escape_bytes_into(
    input: &[u8],
    preserve_layout_controls: bool,
    output: &mut String,
) -> Result<usize, EscapeError> {
    let escaped_len = escaped_bytes_len(input, preserve_layout_controls)?;
    reserve_escape_capacity(output, escaped_len)?;
    output.clear();
    push_escaped_bytes(input, preserve_layout_controls, output);
    debug_assert_eq!(output.len(), escaped_len);
    Ok(escaped_len)
}

fn push_escaped_bytes(mut input: &[u8], preserve_layout_controls: bool, output: &mut String) {
    while !input.is_empty() {
        match std::str::from_utf8(input) {
            Ok(valid) => {
                push_valid_text(output, valid, preserve_layout_controls);
                break;
            }
            Err(error) => {
                let valid_len = error.valid_up_to();
                if valid_len > 0 {
                    // `valid_up_to` は UTF-8 boundary を保証する。
                    if let Ok(valid) = std::str::from_utf8(&input[..valid_len]) {
                        push_valid_text(output, valid, preserve_layout_controls);
                    }
                }

                let invalid_len = error
                    .error_len()
                    .unwrap_or_else(|| input.len().saturating_sub(valid_len));
                // Exact-length passが同じUTF-8 boundaryを検証済み。ここからはerrorを返さないため、
                // output.clear後に失敗して旧内容を失うpathを作らない。
                let invalid_end = valid_len.saturating_add(invalid_len).min(input.len());
                for &byte in &input[valid_len..invalid_end] {
                    push_hex_escape(output, byte);
                }
                input = &input[invalid_end..];
            }
        }
    }
}

fn escaped_bytes_len(
    mut input: &[u8],
    preserve_layout_controls: bool,
) -> Result<usize, EscapeError> {
    let input_len = input.len();
    let mut escaped_len = 0_usize;
    while !input.is_empty() {
        match std::str::from_utf8(input) {
            Ok(valid) => {
                escaped_len = checked_valid_text_len(
                    escaped_len,
                    valid,
                    preserve_layout_controls,
                    input_len,
                )?;
                break;
            }
            Err(error) => {
                let valid_len = error.valid_up_to();
                if valid_len > 0 {
                    let valid = std::str::from_utf8(&input[..valid_len])
                        .map_err(|_| EscapeError::InvalidUtf8Boundary)?;
                    escaped_len = checked_valid_text_len(
                        escaped_len,
                        valid,
                        preserve_layout_controls,
                        input_len,
                    )?;
                }
                let invalid_len = error
                    .error_len()
                    .unwrap_or_else(|| input.len().saturating_sub(valid_len));
                let invalid_end = valid_len
                    .checked_add(invalid_len)
                    .ok_or(EscapeError::LengthOverflow { input_len })?
                    .min(input.len());
                let expanded = invalid_end
                    .saturating_sub(valid_len)
                    .checked_mul(4)
                    .ok_or(EscapeError::LengthOverflow { input_len })?;
                escaped_len = escaped_len
                    .checked_add(expanded)
                    .ok_or(EscapeError::LengthOverflow { input_len })?;
                input = &input[invalid_end..];
            }
        }
    }
    Ok(escaped_len)
}

fn checked_valid_text_len(
    mut escaped_len: usize,
    valid: &str,
    preserve_layout_controls: bool,
    input_len: usize,
) -> Result<usize, EscapeError> {
    for character in valid.chars() {
        let character_len = character.len_utf8();
        let codepoint = u32::from(character);
        let allowed_layout_control = preserve_layout_controls && matches!(character, '\n' | '\t');
        let rendered_len = if allowed_layout_control
            || (!character.is_control() && !(0x7f..=0x9f).contains(&codepoint))
        {
            character_len
        } else {
            character_len
                .checked_mul(4)
                .ok_or(EscapeError::LengthOverflow { input_len })?
        };
        escaped_len = escaped_len
            .checked_add(rendered_len)
            .ok_or(EscapeError::LengthOverflow { input_len })?;
    }
    Ok(escaped_len)
}

fn reserve_escape_capacity(output: &mut String, requested: usize) -> Result<(), EscapeError> {
    if output.capacity() >= requested {
        return Ok(());
    }
    // この分岐ではrequested > capacity >= lenなのでsubtractionはunderflowしない。
    let additional = requested - output.len();
    output
        .try_reserve_exact(additional)
        .map_err(|_| EscapeError::Allocation { requested })
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
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    // Callerが正確な出力長を確保済みなので、4文字のpushはallocationを発生させない。
    output.push('\\');
    output.push('x');
    output.push(char::from(HEX[usize::from(byte >> 4)]));
    output.push(char::from(HEX[usize::from(byte & 0x0f)]));
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn indexed_output_reports_originating_raw_offsets() {
        let mut stripper = AnsiStripper::default();
        let mut output = Vec::new();
        stripper
            .push_indexed(100, b"a\x1b[31mb\n", |offset, byte| {
                output.push((offset, byte));
            })
            .unwrap();
        assert_eq!(output, vec![(100, b'a'), (106, b'b'), (107, b'\n')]);
    }

    #[test]
    fn indexed_output_does_not_treat_osc_newline_as_line_end() {
        let mut stripper = AnsiStripper::default();
        let mut output = Vec::new();
        stripper
            .push_indexed(0, b"a\x1b]x\ny\x07b\n", |offset, byte| {
                output.push((offset, byte));
            })
            .unwrap();
        assert_eq!(output, vec![(0, b'a'), (7, b'b'), (8, b'\n')]);
    }

    #[test]
    fn offset_overflow_is_rejected_before_stripper_state_changes() {
        let mut stripper = AnsiStripper::default();
        assert_eq!(
            stripper.push_indexed(u64::MAX, b"\x1b[", |_, _| {}),
            Err(RawOffsetOverflow)
        );
        let mut output = Vec::new();
        stripper.push(b"x", &mut output);
        assert_eq!(output, b"x");
    }

    #[test]
    fn constrained_utf8_leads_protect_only_valid_c1_continuations() {
        let valid = [
            0xe0, 0xa0, 0x9b, 0xed, 0x80, 0x9b, 0xf0, 0x90, 0x80, 0x9b, 0xf4, 0x8f, 0xbf, 0x9b,
        ];
        assert_eq!(strip_ansi(&valid), valid);

        assert_eq!(strip_ansi(b"\xe0\x9b31mx"), b"\xe0x");
        assert_eq!(strip_ansi(b"\xed\xa0\x9b31mx"), b"\xed\xa0x");
        // F0の2byte目は90..BFなので9Bを直ちにCSIと断定できない。3byte目の`3`でinvalidと
        // 分かった時点で9B以降をCSIとしてreplayし、lead F0だけをtextとして残す。
        assert_eq!(strip_ansi(b"\xf0\x9b31mx"), b"\xf0x");
        assert_eq!(strip_ansi(b"\xf4\x90payload\x9cx"), b"\xf4x");
    }

    #[test]
    fn utf8_validation_survives_chunk_boundaries_next_to_c1() {
        let mut invalid = AnsiStripper::default();
        let mut invalid_output = Vec::new();
        invalid.push(b"\xe0", &mut invalid_output);
        invalid.push(b"\x9b31mx", &mut invalid_output);
        assert_eq!(invalid_output, b"\xe0x");

        let mut valid = AnsiStripper::default();
        let mut valid_output = Vec::new();
        valid.push(b"\xf0", &mut valid_output);
        valid.push(b"\x90", &mut valid_output);
        valid.push(b"\x80\x9b", &mut valid_output);
        assert_eq!(valid_output, b"\xf0\x90\x80\x9b");

        let mut replayed = AnsiStripper::default();
        let mut replayed_output = Vec::new();
        replayed.push(b"\xf0\x9b", &mut replayed_output);
        replayed.push(b"31mx", &mut replayed_output);
        assert_eq!(replayed_output, b"\xf0x");
    }

    #[test]
    fn string_escape_transition_starts_utf8_validation() {
        let input = b"\x1b]title\x1b\xe0\xa0\x9c\x07z";
        assert_eq!(strip_ansi(input), b"z");
    }

    #[test]
    fn string_escape_transition_reprocesses_c1_and_bel_terminators() {
        assert_eq!(strip_ansi(b"\x1b]osc\x1b\x9cq"), b"q");
        assert_eq!(strip_ansi(b"\x1b]osc\x1b\x07q"), b"q");
        assert_eq!(strip_ansi(b"\x1bPdcs\x1b\x9cq"), b"q");
        assert_eq!(strip_ansi(b"\x1bXstring\x1b\x9cq"), b"q");
    }

    #[test]
    fn eof_replays_incomplete_utf8_with_original_indexed_offsets() {
        assert_eq!(strip_ansi(b"a\xe0\xa0"), b"a\xe0\xa0");
        assert_eq!(strip_ansi(b"a\xf0\x9b"), b"a\xf0");

        let mut stripper = AnsiStripper::default();
        let mut output = Vec::new();
        stripper
            .push_indexed(20, b"a\xe0\xa0", |offset, byte| {
                output.push((offset, byte));
            })
            .unwrap();
        stripper.finish_indexed(|offset, byte| output.push((offset, byte)));
        assert_eq!(output, vec![(20, b'a'), (21, 0xe0), (22, 0xa0)]);
    }

    #[test]
    fn fallible_escape_reuses_capacity_and_escapes_control_bytes() {
        let mut output = String::with_capacity(64);
        output.push_str("old contents");
        let pointer = output.as_ptr();
        let capacity = output.capacity();

        escape_interactive_bytes_into(b"ok\t\x00\xff\xc2\x85", &mut output).unwrap();

        assert_eq!(output, "ok\t\\x00\\xFF\\xC2\\x85");
        assert_eq!(output.as_ptr(), pointer);
        assert_eq!(output.capacity(), capacity);
    }

    #[test]
    fn escape_reserves_the_exact_mixed_output_length() {
        let input = b"plain\t\x00\xff\xc2\x85";
        let mut output = String::new();
        let escaped_len = escape_interactive_bytes_into_counted(input, &mut output).unwrap();

        assert_eq!(output, "plain\t\\x00\\xFF\\xC2\\x85");
        assert_eq!(escaped_len, output.len());
        assert_eq!(escaped_len, 22);
        assert!(escaped_len < input.len() * 4);
    }

    #[test]
    fn limited_path_escape_checks_expanded_length_before_output() {
        assert_eq!(
            escape_path_bytes_with_limit(b"\0", 3).unwrap(),
            LimitedPathEscape::LimitExceeded { escaped_len: 4 }
        );
        assert_eq!(
            escape_path_bytes_with_limit(b"\0", 4).unwrap(),
            LimitedPathEscape::Escaped("\\x00".to_owned())
        );
    }

    #[test]
    fn checked_escape_length_overflow_does_not_touch_output() {
        let output = String::from("keep me");
        let error = checked_valid_text_len(usize::MAX, "a", true, 1).unwrap_err();

        assert_eq!(error, EscapeError::LengthOverflow { input_len: 1 });
        assert_eq!(output, "keep me");
    }

    #[test]
    fn escape_reservation_failure_preserves_existing_output() {
        let mut output = String::from("keep me");
        let requested = usize::MAX;
        let error = reserve_escape_capacity(&mut output, requested).unwrap_err();

        assert_eq!(error, EscapeError::Allocation { requested });
        assert_eq!(output, "keep me");
    }

    proptest! {
        #[test]
        fn exact_escape_length_matches_rendered_bytes(input in prop::collection::vec(any::<u8>(), 0..4096)) {
            let expected = escaped_bytes_len(&input, true).unwrap();
            let mut output = String::new();
            let actual = escape_interactive_bytes_into_counted(&input, &mut output).unwrap();

            prop_assert_eq!(actual, expected);
            prop_assert_eq!(actual, output.len());
        }

        #[test]
        fn streaming_escape_matches_eager_across_arbitrary_byte_boundaries(
            input in prop::collection::vec(any::<u8>(), 0..4096),
            chunk_size in 1_usize..65,
        ) {
            let expected = escape_interactive_bytes(&input);
            let mut escaper = StreamingInteractiveEscaper::default();
            let mut actual = String::new();
            for chunk in input.chunks(chunk_size) {
                let _ = escaper.push_into(chunk, &mut actual).unwrap();
            }
            let _ = escaper.finish_into(&mut actual).unwrap();
            prop_assert_eq!(actual, expected);
        }

        #[test]
        fn indexed_and_plain_streaming_paths_have_byte_parity(
            chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 0..64)
        ) {
            let mut plain = AnsiStripper::default();
            let mut indexed = AnsiStripper::default();
            let mut plain_output = Vec::new();
            let mut indexed_output = Vec::new();
            let mut raw_base = 0_u64;

            for chunk in chunks {
                plain.push(&chunk, &mut plain_output);
                indexed
                    .push_indexed(raw_base, &chunk, |offset, byte| {
                        indexed_output.push((offset, byte));
                    })
                    .unwrap();
                raw_base += chunk.len() as u64;
            }
            plain.finish_into(&mut plain_output);
            indexed.finish_indexed(|offset, byte| indexed_output.push((offset, byte)));

            prop_assert_eq!(
                plain_output,
                indexed_output.iter().map(|(_, byte)| *byte).collect::<Vec<_>>()
            );
            prop_assert!(indexed_output.windows(2).all(|pair| pair[0].0 < pair[1].0));
        }
    }
}
