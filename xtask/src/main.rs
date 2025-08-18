// Copyright 2025 V Kontakte LLC
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use lexopt::Parser;
use std::process::Command;

type BoxedError = Box<dyn std::error::Error + Send + Sync>;

fn main() -> Result<(), BoxedError> {
    use lexopt::prelude::*;

    let mut parser = Parser::from_env();
    let subcommand = if let Some(Value(cmd)) = parser.next()? {
        Some(cmd.to_string_lossy().into_owned())
    } else {
        None
    };

    match subcommand.as_deref() {
        Some("bench") => run_bench(&mut parser),
        Some("ci") => run_ci(&mut parser),
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

fn run_bench(parser: &mut Parser) -> Result<(), BoxedError> {
    assert_no_more_args(parser)?;

    let status = Command::new("rustup")
        .args([
            "run",
            "nightly",
            "cargo",
            "bench",
            "--features=_bench",
            "--quiet",
            "--",
            "benches",
        ])
        .status()?;

    if !status.success() {
        return Err(format!("bench command failed with status: {status}").into());
    }

    Ok(())
}

fn run_ci(parser: &mut Parser) -> Result<(), BoxedError> {
    assert_no_more_args(parser)?;

    // Run Go test
    let status = Command::new("go")
        .args([
            "run",
            "github.com/VKCOM/statshouse/cmd/statshouse-client-test@master",
        ])
        .status()?;

    if !status.success() {
        return Err(format!("go test command failed with status: {status}").into());
    }
    // Run cargo fmt check
    let status = Command::new("cargo").args(["fmt", "--check"]).status()?;

    if !status.success() {
        return Err(format!("cargo fmt check failed with status: {status}").into());
    }
    // Run cargo clippy with warnings as errors
    let status = Command::new("cargo")
        .args(["clippy", "--", "-D", "warnings"])
        .status()?;

    if !status.success() {
        return Err(format!("cargo clippy failed with status: {status}").into());
    }
    // Run cargo test
    let status = Command::new("cargo").args(["test", "--quiet"]).status()?;

    if !status.success() {
        return Err(format!("cargo test failed with status: {status}").into());
    }
    // Run nightly bench (compile only)
    let status = Command::new("rustup")
        .args(["run", "nightly", "cargo", "bench", "--no-run", "--quiet"])
        .status()?;

    if !status.success() {
        return Err(format!("nightly bench command failed with status: {status}").into());
    }

    Ok(())
}
