//! `git-ultrapager` binary の process entry point。

#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::expect_used, clippy::unwrap_used))]

fn main() -> std::process::ExitCode {
    git_ultrapager::run()
}
