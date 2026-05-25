//! Tests for parsing zebrad commands

use abscissa_core::config::Override;
use clap::Parser;

use crate::commands::ZebradCmd;

use super::EntryPoint;

#[test]
fn args_with_subcommand_pass_through() {
    let test_cases = [
        (false, true, false, vec!["zebrad"]),
        (false, true, true, vec!["zebrad", "-v"]),
        (false, true, true, vec!["zebrad", "--verbose"]),
        (true, false, false, vec!["zebrad", "-h"]),
        (true, false, false, vec!["zebrad", "--help"]),
        (false, true, false, vec!["zebrad", "start"]),
        (false, true, true, vec!["zebrad", "-v", "start"]),
        (false, true, false, vec!["zebrad", "--filters", "warn"]),
        (true, false, false, vec!["zebrad", "warn"]),
        (false, true, false, vec!["zebrad", "start", "warn"]),
        (true, false, false, vec!["zebrad", "help", "warn"]),
    ];

    for (should_exit, should_be_start, should_be_verbose, args) in test_cases {
        let args = EntryPoint::process_cli_args(args.iter().map(Into::into).collect());

        if should_exit {
            args.expect_err("parsing invalid args or 'help'/'--help' should return an error");
            continue;
        }

        let args: Vec<std::ffi::OsString> = args.expect("args should parse into EntryPoint");

        let args =
            EntryPoint::try_parse_from(args).expect("hardcoded args should parse successfully");

        assert!(args.config.is_none(), "args.config should be none");
        assert!(args.cmd.is_some(), "args.cmd should not be none");
        assert_eq!(
            args.verbose, should_be_verbose,
            "process_cli_args should preserve top-level args"
        );

        assert_eq!(matches!(args.cmd(), ZebradCmd::Start(_)), should_be_start,);
    }
}

#[test]
fn start_checkpoint_arg_parses() {
    let hash = "00040fe8ec8471911baa1db1266ea15dd06b4a8a5c453883c000b031973dce08";
    let args =
        EntryPoint::try_parse_from(["zebrad", "start", "--checkpoint", &format!("0:{hash}")])
            .expect("checkpoint arg should parse successfully");

    let ZebradCmd::Start(cmd) = args.cmd() else {
        panic!("expected start command");
    };

    let config = cmd
        .override_config(crate::config::ZebradConfig::default())
        .expect("checkpoint arg should override config successfully");

    assert_eq!(config.consensus.extra_checkpoints.len(), 1);
    assert_eq!(config.consensus.extra_checkpoints[0].height, 0);
    assert_eq!(config.consensus.extra_checkpoints[0].hash, hash);
}
