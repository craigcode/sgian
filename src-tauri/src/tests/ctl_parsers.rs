use super::*;

// ----- parse_exec_args: pure option parser for `ctl exec` -----

pub(crate) fn exec_args(items: &[&str]) -> Vec<String> {
    items.iter().map(ToString::to_string).collect()
}

// ----- has_help_flag: pre-`--` help detection -----

#[test]
fn has_help_flag_detects_bare_help() {
    assert!(has_help_flag(&exec_args(&["--help"])));
}

#[test]
fn has_help_flag_detects_bare_short_help() {
    assert!(has_help_flag(&exec_args(&["-h"])));
}

#[test]
fn has_help_flag_detects_help_before_separator() {
    assert!(has_help_flag(&exec_args(&["--pane", "x", "--help"])));
}

#[test]
fn has_help_flag_empty_args() {
    assert!(!has_help_flag(&[]));
}

#[test]
fn has_help_flag_ignores_help_after_separator() {
    assert!(!has_help_flag(&exec_args(&["--", "echo", "--help"])));
}

#[test]
fn has_help_flag_ignores_short_help_after_separator() {
    assert!(!has_help_flag(&exec_args(&["--", "echo", "-h"])));
}

#[test]
fn has_help_flag_ignores_help_right_after_separator() {
    assert!(!has_help_flag(&exec_args(&["--", "--help"])));
}

#[test]
fn has_help_flag_ignores_short_help_right_after_separator() {
    assert!(!has_help_flag(&exec_args(&["--", "-h"])));
}

#[test]
fn has_help_flag_ignores_help_after_separator_with_options() {
    assert!(!has_help_flag(&exec_args(&[
        "--pane", "x", "--", "echo", "--help"
    ])));
}

#[test]
fn parse_exec_args_defaults_to_active_with_bare_command() {
    let plan = parse_exec_args(&exec_args(&["echo", "hi"])).expect("bare command should parse");
    assert!(!plan.create_new);
    assert!(!plan.all);
    assert_eq!(plan.pane_ref, None);
    assert_eq!(plan.panes_list, None);
    assert_eq!(plan.title, None);
    assert_eq!(plan.command, "echo hi");
}

#[test]
fn parse_exec_args_parses_new_and_name() {
    let plan = parse_exec_args(&exec_args(&[
        "--new", "--name", "build", "--", "echo", "hi",
    ]))
    .expect("new+name should parse");
    assert!(plan.create_new);
    assert_eq!(plan.title, Some("build".to_string()));
    assert_eq!(plan.command, "echo hi");
}

#[test]
fn parse_exec_args_name_alias_n() {
    let plan = parse_exec_args(&exec_args(&["-n", "build", "--", "echo", "hi"]))
        .expect("-n alias should parse");
    assert_eq!(plan.title, Some("build".to_string()));
}

#[test]
fn parse_exec_args_parses_all() {
    let plan =
        parse_exec_args(&exec_args(&["--all", "--", "echo", "hi"])).expect("--all should parse");
    assert!(plan.all);
    assert_eq!(plan.command, "echo hi");
}

#[test]
fn parse_exec_args_parses_panes_list() {
    let plan = parse_exec_args(&exec_args(&["--panes", "a,b", "--", "echo", "hi"]))
        .expect("--panes should parse");
    assert_eq!(plan.panes_list, Some("a,b".to_string()));
}

#[test]
fn parse_exec_args_parses_pane_ref() {
    let plan = parse_exec_args(&exec_args(&["--pane", "build", "--", "echo", "hi"]))
        .expect("--pane should parse");
    assert_eq!(plan.pane_ref, Some("build".to_string()));
}

#[test]
fn parse_exec_args_joins_multi_word_command() {
    let plan = parse_exec_args(&exec_args(&["--", "sh", "-c", "exit 7"]))
        .expect("multi-word command should parse");
    assert_eq!(plan.command, "sh -c exit 7");
}

#[test]
fn parse_exec_args_errors_on_missing_command() {
    let err = parse_exec_args(&exec_args(&["--new"])).expect_err("should error");
    assert!(err.contains("exec requires a command"));
}

#[test]
fn parse_exec_args_errors_on_unknown_option() {
    let err = parse_exec_args(&exec_args(&["--bogus", "echo"])).expect_err("should error");
    assert!(err.contains("unknown exec option"));
    assert!(err.contains("--bogus"));
}

#[test]
fn parse_exec_args_errors_on_missing_pane_value() {
    let err = parse_exec_args(&exec_args(&["--pane"])).expect_err("should error");
    assert!(err.contains("--pane requires a pane"));
}

#[test]
fn parse_exec_args_errors_on_missing_panes_value() {
    let err = parse_exec_args(&exec_args(&["--panes"])).expect_err("should error");
    assert!(err.contains("--panes requires"));
}

#[test]
fn parse_exec_args_errors_on_missing_name_value() {
    let err = parse_exec_args(&exec_args(&["--name"])).expect_err("should error");
    assert!(err.contains("--name requires a title"));
}

// 07-19 CLI low: exec mirrors run's targeting-flag exclusivity instead of
// silently letting --all override --panes override --pane.

#[test]
fn parse_exec_args_rejects_all_with_pane() {
    let err = parse_exec_args(&exec_args(&["--all", "--pane", "x", "--", "true"]))
        .expect_err("--all + --pane should error");
    assert!(
        err.contains("cannot be combined"),
        "unexpected error: {err}"
    );
    let err = parse_exec_args(&exec_args(&["--pane", "x", "--all", "--", "true"]))
        .expect_err("--pane + --all should error (order-independent)");
    assert!(
        err.contains("cannot be combined"),
        "unexpected error: {err}"
    );
}

#[test]
fn parse_exec_args_rejects_all_with_panes() {
    let err = parse_exec_args(&exec_args(&["--all", "--panes", "a,b", "--", "true"]))
        .expect_err("--all + --panes should error");
    assert!(
        err.contains("cannot be combined"),
        "unexpected error: {err}"
    );
}

#[test]
fn parse_exec_args_rejects_panes_with_pane() {
    let err = parse_exec_args(&exec_args(&["--panes", "a,b", "--pane", "x", "--", "true"]))
        .expect_err("--panes + --pane should error");
    assert!(
        err.contains("cannot be combined"),
        "unexpected error: {err}"
    );
}

#[test]
fn parse_exec_args_rejects_new_and_name_under_batched_targeting() {
    // --new/--name only apply to the single-pane path; under --all/--panes
    // they used to be silently dropped.
    let err = parse_exec_args(&exec_args(&["--all", "--new", "--", "true"]))
        .expect_err("--all + --new should error");
    assert!(
        err.contains("cannot be combined"),
        "unexpected error: {err}"
    );
    let err = parse_exec_args(&exec_args(&["--panes", "a", "--name", "t", "--", "true"]))
        .expect_err("--panes + --name should error");
    assert!(
        err.contains("cannot be combined"),
        "unexpected error: {err}"
    );
    let err = parse_exec_args(&exec_args(&["--new", "--all", "--", "true"]))
        .expect_err("--new + --all should error (order-independent)");
    assert!(
        err.contains("cannot be combined"),
        "unexpected error: {err}"
    );
    // --new + --name + --pane remains valid (single-pane targeting).
    parse_exec_args(&exec_args(&[
        "--new", "--name", "t", "--pane", "x", "--", "true",
    ]))
    .expect("--new + --name + --pane should parse");
}

#[test]
fn parse_exec_args_rejects_empty_panes_list() {
    // 07-19 CLI low: `--panes ""` / `--panes ","` targeted zero panes and
    // exited 0 vacuously — a usage error instead.
    let err = parse_exec_args(&exec_args(&["--panes", "", "--", "true"]))
        .expect_err("empty --panes should error");
    assert!(err.contains("at least one pane"), "unexpected error: {err}");
    let err = parse_exec_args(&exec_args(&["--panes", ",", "--", "true"]))
        .expect_err("commas-only --panes should error");
    assert!(err.contains("at least one pane"), "unexpected error: {err}");
    let err = parse_exec_args(&exec_args(&["--panes", " , ", "--", "true"]))
        .expect_err("whitespace-only --panes should error");
    assert!(err.contains("at least one pane"), "unexpected error: {err}");
    // Trailing/empty items around a real pane are still fine.
    let plan = parse_exec_args(&exec_args(&["--panes", "a,", "--", "true"]))
        .expect("a named pane with an empty item should parse");
    assert_eq!(plan.panes_list.as_deref(), Some("a,"));
}

// ----- parse_logs_args: pure option parser for `ctl logs` -----

#[test]
fn parse_logs_args_defaults_to_all_lines_no_follow() {
    let plan = parse_logs_args(&[]).expect("no args should parse");
    assert_eq!(
        plan,
        LogsPlan {
            lines: None,
            follow: false
        }
    );
}

#[test]
fn parse_logs_args_parses_short_lines_flag() {
    let plan = parse_logs_args(&exec_args(&["-n", "5"])).expect("-n should parse");
    assert_eq!(plan.lines, Some(5));
    assert!(!plan.follow);
}

#[test]
fn parse_logs_args_parses_long_lines_flag() {
    let plan = parse_logs_args(&exec_args(&["--lines", "10"])).expect("--lines should parse");
    assert_eq!(plan.lines, Some(10));
    assert!(!plan.follow);
}

#[test]
fn parse_logs_args_parses_follow_long_flag() {
    let plan = parse_logs_args(&exec_args(&["--follow"])).expect("--follow should parse");
    assert!(plan.follow);
    assert_eq!(plan.lines, None);
}

#[test]
fn parse_logs_args_parses_follow_short_flag() {
    let plan = parse_logs_args(&exec_args(&["-f"])).expect("-f should parse");
    assert!(plan.follow);
}

#[test]
fn parse_logs_args_parses_combined_lines_and_follow() {
    let plan =
        parse_logs_args(&exec_args(&["-n", "3", "--follow"])).expect("combined flags should parse");
    assert_eq!(plan.lines, Some(3));
    assert!(plan.follow);
}

#[test]
fn parse_logs_args_errors_on_invalid_line_count() {
    let err = parse_logs_args(&exec_args(&["-n", "abc"])).expect_err("should error");
    assert!(err.contains("invalid line count"));
}

#[test]
fn parse_logs_args_errors_on_missing_line_count() {
    let err = parse_logs_args(&exec_args(&["-n"])).expect_err("should error");
    assert!(err.contains("requires a line count"));
}

#[test]
fn parse_logs_args_errors_on_unknown_option() {
    let err = parse_logs_args(&exec_args(&["--bogus"])).expect_err("should error");
    assert!(err.contains("unknown logs option"));
}

// ----- read_log_tail: pure I/O helper for ctl logs -----

#[test]
fn read_log_tail_returns_all_lines_when_no_limit() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("test.log");
    fs::write(&path, "line1\nline2\nline3\n").expect("write log");
    let lines = read_log_tail(&path, None);
    assert_eq!(lines, vec!["line1", "line2", "line3"]);
}

#[test]
fn read_log_tail_limits_to_last_n_lines() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("test.log");
    fs::write(&path, "line1\nline2\nline3\nline4\nline5\n").expect("write log");
    let lines = read_log_tail(&path, Some(2));
    assert_eq!(lines, vec!["line4", "line5"]);
}

#[test]
fn read_log_tail_limit_one_returns_last_line() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("test.log");
    fs::write(&path, "line1\nline2\nline3\n").expect("write log");
    let lines = read_log_tail(&path, Some(1));
    assert_eq!(lines, vec!["line3"]);
}

#[test]
fn read_log_tail_limit_exceeding_count_returns_all() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("test.log");
    fs::write(&path, "line1\nline2\n").expect("write log");
    let lines = read_log_tail(&path, Some(10));
    assert_eq!(lines, vec!["line1", "line2"]);
}

#[test]
fn read_log_tail_missing_file_returns_empty() {
    let lines = read_log_tail(Path::new("/nonexistent/path/log.log"), None);
    assert!(lines.is_empty());
}

// ----- parse_run_args: pure option parser for `ctl run` -----

#[test]
fn parse_run_args_defaults_to_active_pane() {
    let plan = parse_run_args(&exec_args(&["--", "echo", "hi"])).expect("bare run should parse");
    assert_eq!(plan.pane_ref, "active");
    assert_eq!(plan.command_args, vec!["echo", "hi"]);
    assert_eq!(plan.timeout_ms, None);
}

#[test]
fn parse_run_args_parses_pane_flag() {
    let plan = parse_run_args(&exec_args(&["--pane", "build", "--", "echo", "hi"]))
        .expect("--pane should parse");
    assert_eq!(plan.pane_ref, "build");
    assert_eq!(plan.command_args, vec!["echo", "hi"]);
}

#[test]
fn parse_run_args_parses_timeout() {
    let plan = parse_run_args(&exec_args(&["--timeout", "2500", "--", "true"]))
        .expect("--timeout should parse");
    assert_eq!(plan.timeout_ms, Some(2500));

    let err = parse_run_args(&exec_args(&["--timeout", "soon", "--", "true"]))
        .expect_err("non-numeric timeout should error");
    assert!(err.contains("--timeout requires a non-negative integer"));
    let err = parse_run_args(&exec_args(&["--timeout"])).expect_err("missing value should error");
    assert!(err.contains("--timeout requires a value"));
}

#[test]
fn parse_run_args_keeps_raw_args_and_quotes_at_family_time() {
    let plan = parse_run_args(&exec_args(&["--", "sh", "-c", "exit 42"]))
        .expect("multi-word command should parse");
    // The parser keeps raw tokens; quoting happens once the shell family is
    // known. POSIX quoting preserves the user's argument grouping: `exit 42`
    // stays a single argument to `-c` instead of splitting into `exit` + `$0=42`.
    assert_eq!(plan.command_args, vec!["sh", "-c", "exit 42"]);
    assert_eq!(
        quote_command_for(ShellFamily::Posix, &plan.command_args),
        "sh -c 'exit 42'"
    );
}

#[test]
fn parse_run_args_errors_on_missing_command() {
    let err = parse_run_args(&exec_args(&["--pane", "active"])).expect_err("should error");
    assert!(err.contains("run requires a command"));
}

#[test]
fn parse_run_args_errors_on_unknown_option() {
    let err = parse_run_args(&exec_args(&["--bogus", "echo"])).expect_err("should error");
    assert!(err.contains("unknown run option"));
    assert!(err.contains("--bogus"));
}

#[test]
fn parse_run_args_errors_on_missing_pane_value() {
    let err = parse_run_args(&exec_args(&["--pane"])).expect_err("should error");
    assert!(err.contains("--pane requires a pane"));
}

#[test]
fn parse_run_args_parses_all_flag() {
    let plan = parse_run_args(&exec_args(&["--all", "--", "true"])).expect("should parse");
    assert!(plan.all);
    assert_eq!(plan.panes_list, None);
    assert_eq!(plan.pane_ref, "active");
    assert_eq!(plan.command_args, vec!["true"]);
}

#[test]
fn parse_run_args_parses_panes_list() {
    let plan =
        parse_run_args(&exec_args(&["--panes", "a,b", "--", "echo", "hi"])).expect("should parse");
    assert!(!plan.all);
    assert_eq!(plan.panes_list.as_deref(), Some("a,b"));
    assert_eq!(plan.command_args, vec!["echo", "hi"]);
}

#[test]
fn parse_run_args_errors_on_missing_panes_value() {
    let err = parse_run_args(&exec_args(&["--panes"])).expect_err("should error");
    assert!(err.contains("--panes requires"));
}

#[test]
fn parse_run_args_rejects_all_with_pane() {
    parse_run_args(&exec_args(&["--all", "--pane", "x", "--", "true"]))
        .expect_err("--all + --pane should error");
}

#[test]
fn parse_run_args_rejects_all_with_panes() {
    parse_run_args(&exec_args(&["--all", "--panes", "a,b", "--", "true"]))
        .expect_err("--all + --panes should error");
}

#[test]
fn parse_run_args_rejects_panes_with_pane() {
    parse_run_args(&exec_args(&["--panes", "a,b", "--pane", "x", "--", "true"]))
        .expect_err("--panes + --pane should error");
}

#[test]
fn parse_run_args_rejects_empty_panes_list() {
    // 07-19 CLI low: `--panes ""` / `--panes ","` resolved to an empty
    // target set and collect_batched_results exited 0 vacuously.
    let err = parse_run_args(&exec_args(&["--panes", "", "--", "true"]))
        .expect_err("empty --panes should error");
    assert!(err.contains("at least one pane"), "unexpected error: {err}");
    let err = parse_run_args(&exec_args(&["--panes", ",", "--", "true"]))
        .expect_err("commas-only --panes should error");
    assert!(err.contains("at least one pane"), "unexpected error: {err}");
    // A real pane amid empty items is still accepted.
    let plan = parse_run_args(&exec_args(&["--panes", "a,,b", "--", "true"]))
        .expect("named panes with an empty item should parse");
    assert_eq!(plan.panes_list.as_deref(), Some("a,,b"));
}

// ----- shell_quote (arg-grouping fix for ctl run) -----

#[test]
fn shell_quote_leaves_safe_args_unquoted() {
    assert_eq!(shell_quote("echo"), "echo");
    assert_eq!(shell_quote("/usr/bin/false"), "/usr/bin/false");
    assert_eq!(shell_quote("exit"), "exit");
    assert_eq!(shell_quote("a-b_c.d=e,f@g+h"), "a-b_c.d=e,f@g+h");
}

#[test]
fn shell_arg_is_safe_rejects_leading_equals_only() {
    // 07-19 CLI low: zsh equals-expansion makes a word-initial `=`
    // (`ctl run -- =foo`) expand to the path of `foo`; mid-word `=` keeps
    // its literal assignment shape.
    assert!(!shell_arg_is_safe("=foo"));
    assert!(!shell_arg_is_safe("="));
    assert!(shell_arg_is_safe("FOO=bar"));
    assert!(shell_arg_is_safe("a="));
    assert_eq!(shell_quote("=foo"), "'=foo'");
    assert_eq!(shell_quote("FOO=bar"), "FOO=bar");
    // Same predicate guards the fish quoter.
    assert_eq!(fish_quote("=foo"), "'=foo'");
    assert_eq!(fish_quote("FOO=bar"), "FOO=bar");
}

#[test]
fn shell_quote_quotes_args_with_spaces() {
    assert_eq!(shell_quote("exit 7"), "'exit 7'");
    assert_eq!(shell_quote("exit 42"), "'exit 42'");
    assert_eq!(shell_quote("echo hello world"), "'echo hello world'");
}

#[test]
fn shell_quote_quotes_empty_arg() {
    assert_eq!(shell_quote(""), "''");
}

#[test]
fn shell_quote_escapes_embedded_single_quotes() {
    // `it's a test` → `'it'\''s a test'`
    assert_eq!(shell_quote("it's a test"), "'it'\\''s a test'");
}

#[test]
fn shell_quote_quotes_shell_metacharacters() {
    assert_eq!(shell_quote("$HOME"), "'$HOME'");
    assert_eq!(shell_quote("a;b"), "'a;b'");
    assert_eq!(shell_quote("a|b"), "'a|b'");
    assert_eq!(shell_quote("*"), "'*'");
    assert_eq!(shell_quote("`cmd`"), "'`cmd`'");
}

#[test]
fn quote_command_for_posix_preserves_grouping() {
    // The arg-grouping bug: `run -- sh -c 'exit 7'` was joined as
    // `sh -c exit 7` which the pane shell parses as `sh -c exit` with
    // `$0=7`. Shell-quoting preserves `exit 7` as a single argument.
    let plan = parse_run_args(&exec_args(&["--", "sh", "-c", "exit 7"])).expect("should parse");
    assert_eq!(
        quote_command_for(ShellFamily::Posix, &plan.command_args),
        "sh -c 'exit 7'"
    );

    let plan = parse_run_args(&exec_args(&["--", "sh", "-c", "echo hello; exit 3"]))
        .expect("should parse");
    assert_eq!(
        quote_command_for(ShellFamily::Posix, &plan.command_args),
        "sh -c 'echo hello; exit 3'"
    );
}

#[test]
fn fish_quote_escapes_fish_specials() {
    // Fish single quotes treat only \ and ' as special (backslash-escaped).
    assert_eq!(fish_quote("plain"), "plain");
    assert_eq!(fish_quote(""), "''");
    assert_eq!(fish_quote("has space"), "'has space'");
    assert_eq!(fish_quote("it's"), "'it\\'s'");
    // A backslash-bearing arg: POSIX quoting would pass `\` through
    // untouched inside '…', but fish interprets `\'`/`\\` inside single
    // quotes — so fish quoting must double the backslashes (H7).
    assert_eq!(fish_quote(r"C:\tmp\"), r"'C:\\tmp\\'");
    assert_eq!(
        quote_command_for(ShellFamily::Fish, &["echo".to_string(), r"a\b".to_string()]),
        r"echo 'a\\b'"
    );
}

// ----- detect_shell_family (shell-aware exit codes, VAL-ORCH-006) -----

#[test]
fn detect_shell_family_posix_shells() {
    assert_eq!(detect_shell_family("/bin/sh"), ShellFamily::Posix);
    assert_eq!(detect_shell_family("/bin/bash"), ShellFamily::Posix);
    assert_eq!(detect_shell_family("/bin/zsh"), ShellFamily::Posix);
    assert_eq!(detect_shell_family("/bin/dash"), ShellFamily::Posix);
    assert_eq!(detect_shell_family("/usr/bin/sh"), ShellFamily::Posix);
}

#[test]
fn detect_shell_family_fish() {
    assert_eq!(detect_shell_family("fish"), ShellFamily::Fish);
    assert_eq!(detect_shell_family("/usr/bin/fish"), ShellFamily::Fish);
    assert_eq!(
        detect_shell_family("/opt/homebrew/bin/fish"),
        ShellFamily::Fish
    );
    // A path whose basename starts with "fish" (e.g. fish-git) is also Fish.
    assert_eq!(
        detect_shell_family("/usr/local/bin/fish-dev"),
        ShellFamily::Fish
    );
}

#[test]
fn detect_shell_family_unknown_shell_defaults_to_posix() {
    assert_eq!(detect_shell_family("/bin/ksh"), ShellFamily::Posix);
    assert_eq!(detect_shell_family("/bin/tcsh"), ShellFamily::Posix);
    assert_eq!(detect_shell_family(""), ShellFamily::Posix);
}

// ----- build_run_wrapper (shell-aware exit codes, VAL-ORCH-006) -----

#[test]
fn build_run_wrapper_posix_uses_dollar_question() {
    let wrapper = build_run_wrapper("true", "__sgian_rc_42_1700", ShellFamily::Posix);
    assert!(
        wrapper.contains("\"$?\""),
        "POSIX wrapper must use \"$?\": {wrapper}"
    );
    assert!(wrapper.ends_with('\r'), "wrapper must end with CR");
    assert!(
        wrapper.contains("; printf '\\n__sgian_rc_42_1700:%s\\n'"),
        "wrapper must contain marker printf: {wrapper}"
    );
}

#[test]
fn build_run_wrapper_fish_uses_dollar_status() {
    let wrapper = build_run_wrapper("true", "__sgian_rc_42_1700", ShellFamily::Fish);
    assert!(
        wrapper.contains("$status"),
        "fish wrapper must use $status: {wrapper}"
    );
    assert!(
        !wrapper.contains("$?"),
        "fish wrapper must NOT use $?: {wrapper}"
    );
    assert!(wrapper.ends_with('\r'), "wrapper must end with CR");
    assert!(
        wrapper.contains("; printf '\\n__sgian_rc_42_1700:%s\\n'"),
        "wrapper must contain marker printf: {wrapper}"
    );
}

// ----- parse_exit_marker + trim_to_tail under high-volume output -----

#[test]
fn parse_exit_marker_finds_code_after_large_output() {
    let prefix = "__sgian_rc_42_1700000000:";
    // Simulate 100k lines of output followed by the marker line.
    let mut buffer = String::new();
    for i in 0..100_000 {
        buffer.push_str(&format!("line{i}\n"));
    }
    buffer.push_str(&format!("{prefix}5\n"));
    assert_eq!(parse_exit_marker(&buffer, prefix), Some(5));
}

#[test]
fn trim_to_tail_preserves_partial_marker_for_next_chunk() {
    let prefix = "__sgian_rc_42_1700000000:";
    let mut buffer = String::new();
    // Fill with noise, then a partial marker (digits still in flight).
    for i in 0..10_000 {
        buffer.push_str(&format!("line{i}\n"));
    }
    buffer.push_str(prefix);
    buffer.push('4'); // partial code, no terminator yet

    // Simulate the trim that control_run does after each event.
    trim_to_tail(&mut buffer, prefix.len() + 64);

    // The partial marker must survive the trim so the next chunk can
    // complete it.
    assert!(
        buffer.contains(prefix),
        "partial marker prefix must survive trim, got: ...{}",
        &buffer[buffer.len().saturating_sub(80)..]
    );
    assert!(buffer.ends_with('4'));

    // Now append the terminator — the full marker should parse.
    buffer.push('\n');
    assert_eq!(parse_exit_marker(&buffer, prefix), Some(4));
}

#[test]
fn trim_to_tail_repeatedly_keeps_marker_findable() {
    // Simulate the high-volume control_run loop: many events, each
    // trimmed, with the marker arriving only at the end.
    let prefix = "__sgian_rc_99_1700000001:";
    let mut buffer = String::new();
    for chunk in 0..500u32 {
        for i in 0..100 {
            buffer.push_str(&format!("c{chunk}l{i}\n"));
        }
        trim_to_tail(&mut buffer, prefix.len() + 64);
        // Marker not yet arrived.
        assert_eq!(parse_exit_marker(&buffer, prefix), None);
    }
    // Final chunk carries the marker.
    buffer.push_str(&format!("{prefix}42\n"));
    assert_eq!(parse_exit_marker(&buffer, prefix), Some(42));
}

// ----- Multi-subscriber broadcast fan-out (VAL-OBS-022) -----

#[test]
fn broadcast_delivers_to_every_concurrent_subscriber() {
    let scrollback_dir = tempfile::tempdir().expect("scrollback dir");
    let router = OutputRouter::new(scrollback_dir.path().to_path_buf());

    // K subscribers, each backed by a connected transport pair so the
    // fan-out is observable per-subscriber (not just a count).
    const K: usize = 3;
    let mut clients = Vec::new();
    for _ in 0..K {
        let (client, server) = test_transport_pair().expect("transport pair should be available");
        router
            .add_subscriber(server, 1)
            .expect("subscribe within the cap");
        clients.push(client);
    }

    assert_eq!(router.subscriber_count(), K);

    let event = DaemonEvent::PtyOutput {
        pane_id: "pane-1".to_string(),
        data: "fan-out\n".to_string(),
    };
    router.broadcast(&event);

    // EACH of the K subscribers must actually receive the event — proving
    // fan-out, not just bookkeeping.
    for client in clients {
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout should apply");
        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("subscriber should receive the broadcast event");
        let received: DaemonEvent = serde_json::from_str(&line).expect("event should deserialize");
        assert_eq!(received, event);
    }
}

/// M5: subscription beyond MAX_SUBSCRIBERS is refused with a clean error
/// (the stream is handed back and closed — no entry, channel, or threads
/// leak), and a disconnect frees a slot so subscribing succeeds again.
#[cfg(unix)]
#[test]
fn subscriber_cap_rejects_beyond_limit_and_recovers_after_disconnect() {
    let scrollback_dir = tempfile::tempdir().expect("temp scrollback dir");
    let router = OutputRouter::new(scrollback_dir.path().to_path_buf());

    let mut peers = Vec::new();
    for _ in 0..MAX_SUBSCRIBERS {
        let (client, server) = UnixStream::pair().expect("unix stream pair should be available");
        router
            .add_subscriber(server, 1)
            .expect("subscribe within the cap");
        peers.push(client);
    }
    assert_eq!(router.subscriber_count(), MAX_SUBSCRIBERS);

    // The next subscribe is rejected cleanly: the reason and the stream are
    // handed back so the caller can respond and close it (no leak).
    let (client, server) = UnixStream::pair().expect("unix stream pair should be available");
    let (reason, stream) = router
        .add_subscriber(server, 1)
        .expect_err("subscribe beyond the cap should be refused");
    assert!(
        reason.contains("subscriber limit"),
        "unexpected reason: {reason}"
    );
    drop(stream);
    drop(client);
    assert_eq!(
        router.subscriber_count(),
        MAX_SUBSCRIBERS,
        "a rejected subscribe must not register"
    );

    // Disconnect one subscriber; its watcher prunes the entry, and a new
    // subscribe succeeds on the freed slot.
    peers.pop();
    wait_for(|| router.subscriber_count() < MAX_SUBSCRIBERS);
    let (client, server) = UnixStream::pair().expect("unix stream pair should be available");
    router
        .add_subscriber(server, 1)
        .expect("subscribe after a disconnect freed a slot");
    peers.push(client);
}

/// L12 pin: a subscriber that never drains its 1024-slot queue is DROPPED on
/// burst — broadcast fan-out must never block on one slow consumer. With the
/// peer never reading, the socket buffer fills, the writer thread stalls,
/// the bounded queue fills, and a later broadcast's `try_send` fails Full.
#[cfg(unix)]
#[test]
fn slow_subscriber_is_dropped_when_its_queue_fills() {
    let scrollback_dir = tempfile::tempdir().expect("temp scrollback dir");
    let router = OutputRouter::new(scrollback_dir.path().to_path_buf());
    let (client, server) = UnixStream::pair().expect("unix stream pair");
    router
        .add_subscriber(server, 1)
        .expect("subscribe within the cap");
    assert_eq!(router.subscriber_count(), 1);

    // Never read from `client`. Payloads large enough to fill the socket
    // buffer fast make the queue fill deterministically: once it does, the
    // broadcast fan-out drops the subscriber synchronously.
    let event = DaemonEvent::PtyOutput {
        pane_id: "pane-1".to_string(),
        data: "x".repeat(32 * 1024),
    };
    for _ in 0..SUBSCRIBER_QUEUE_LIMIT * 2 {
        router.broadcast(&event);
    }

    wait_for(|| router.subscriber_count() == 0);
    drop(client);
}
