// Copyright 2024 V Kontakte LLC
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use lexopt::Parser;
use xshell::{cmd, Shell};

type BoxedError = Box<dyn std::error::Error + Send + Sync>;

fn main() -> Result<(), BoxedError> {
    use lexopt::prelude::*;

    let mut parser = Parser::from_env();
    let subcommand = if let Some(Value(cmd)) = parser.next()? {
        Some(cmd.to_string_lossy().into_owned())
    } else {
        None
    };

    let sh = Shell::new()?;
    match subcommand.as_deref() {
        Some("bench") => run_bench(&mut parser, &sh),
        Some("ci") => run_ci(&mut parser, &sh),
        Some(cmd) => Err(format!("unexpected subcommand: {cmd}").into()),
        None => Err("subcommand expected".into()),
    }
}

fn assert_no_more_args(parser: &mut Parser) -> Result<(), BoxedError> {
    use lexopt::prelude::*;

    match parser.next()? {
        Some(Short(c)) => Err(format!("unexpected option: -{c}").into()),
        Some(Long(s)) => Err(format!("unexpected option: --{s}").into()),
        Some(Value(v)) => Err(format!("unexpected value: {v:?}").into()),
        None => Ok(()),
    }
}

fn run_bench(parser: &mut Parser, sh: &Shell) -> Result<(), BoxedError> {
    assert_no_more_args(parser)?;

    cmd!(
        sh,
        "cargo +nightly bench --features=_bench --quiet -- benches"
    )
    .run()?;

    Ok(())
}

fn run_ci(parser: &mut Parser, sh: &Shell) -> Result<(), BoxedError> {
    assert_no_more_args(parser)?;

    cmd!(sh, "cargo fmt --check").run()?;
    cmd!(sh, "cargo clippy -- -D warnings").run()?;
    cmd!(sh, "cargo test --quiet").run()?;
    cmd!(sh, "cargo +nightly bench --no-run --quiet").run()?;

    Ok(())
}
