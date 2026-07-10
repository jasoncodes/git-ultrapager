//! Git の byte stream を安全な表示 model へ変換し、direct output と TUI の双方へ提供する。
//!
//! この crate は Git process の起動や repository file の読み取りを行わない。stdin の所有権は
//! input layer、terminal state の所有権は terminal layer に分離し、parser と renderer は
//! terminal escape sequence を副作用として実行しない。

#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::expect_used, clippy::unwrap_used))]

pub mod ansi;
pub mod app;
pub mod cache;
pub mod cli;
pub mod decorations;
pub mod direct;
pub mod document_store;
pub mod highlight;
pub mod input;
pub mod intraline;
pub mod model;
pub mod parser;
pub mod runtime;
pub mod search;
pub mod state;
pub mod storage;
pub mod terminal;
pub mod text;
pub mod theme;
pub mod ui;

use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

/// CLI を実行し、shell へ返す exit status を決定する。
///
/// terminal session を開始した場合も、この関数から戻る前に raw mode、alternate screen、cursor
/// visibility を復元する。usage error は clap が処理し、runtime error は terminal 復元後に stderr
/// へ出力する。
pub fn run() -> ExitCode {
    // Default hook は unwind より先に stderr へ書くため、alternate screen の中へ診断が混ざる。
    // hook では文字列だけを退避し、app stack の TerminalSession が drop された後に出力する。
    let panic_report = Arc::new(Mutex::new(None));
    let hook_report = Arc::clone(&panic_report);
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if let Ok(mut report) = hook_report.lock() {
            *report = Some(format_panic(info));
        }
    }));
    let outcome = panic::catch_unwind(AssertUnwindSafe(app::run));
    panic::set_hook(previous_hook);

    match outcome {
        Ok(Ok(code)) => code,
        Ok(Err(error)) => {
            eprintln!("git-ultrapager: {error:#}");
            ExitCode::FAILURE
        }
        Err(payload) => {
            let captured = panic_report.lock().ok().and_then(|report| report.clone());
            let report = captured.unwrap_or_else(|| panic_payload(&payload).to_owned());
            eprintln!("git-ultrapager: internal error: {report}");
            ExitCode::FAILURE
        }
    }
}

fn format_panic(info: &PanicHookInfo<'_>) -> String {
    let message = panic_payload(info.payload());
    info.location().map_or_else(
        || message.to_owned(),
        |location| {
            format!(
                "{message} at {}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )
        },
    )
}

fn panic_payload(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}
