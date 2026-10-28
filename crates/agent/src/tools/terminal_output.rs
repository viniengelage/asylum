//! Shrinks terminal output before it is sent to the model.
//!
//! The user still sees the full output in the terminal card; only the
//! model-facing copy is compacted. Every filter here drops lines that carry no
//! information for the model (build progress, passing tests, git hints), so a
//! failing command keeps all of its diagnostics.

/// Consecutive identical lines are collapsed once a run reaches this length.
const MIN_REPEATED_RUN: usize = 3;

/// Share of the byte budget kept from the start of the output when it has to
/// be truncated. The rest is kept from the end, where errors and summaries
/// usually are.
const TRUNCATION_HEAD_DIVISOR: usize = 4;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CommandKinds {
    cargo: bool,
    javascript_tests: bool,
    pytest: bool,
    go_test: bool,
    git_status: bool,
}

pub(super) struct CompactedOutput {
    pub text: String,
    pub omitted_lines: usize,
}

pub(super) fn compact_output(command: &str, output: &str) -> CompactedOutput {
    let kinds = command_kinds(command);
    let lines = output.lines().map(str::trim_end).collect::<Vec<_>>();
    let mut omitted_lines = 0;

    let mut kept = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        if is_noise_line(line, kinds, &lines[index + 1..]) {
            omitted_lines += 1;
        } else {
            kept.push(*line);
        }
    }

    let mut text = String::with_capacity(output.len());
    let mut index = 0;
    while let Some(line) = kept.get(index) {
        let run_length = kept[index..]
            .iter()
            .take_while(|other| *other == line)
            .count();
        if line.is_empty() {
            omitted_lines += run_length - 1;
            push_line(&mut text, "");
        } else if run_length >= MIN_REPEATED_RUN {
            omitted_lines += run_length - 1;
            push_line(&mut text, line);
            push_line(
                &mut text,
                &format!("… (previous line repeated {} more times)", run_length - 1),
            );
        } else {
            for _ in 0..run_length {
                push_line(&mut text, line);
            }
        }
        index += run_length;
    }

    CompactedOutput {
        text,
        omitted_lines,
    }
}

/// Keeps the beginning and the end of `output` within `limit` bytes, cutting
/// on line boundaries. Returns `None` when the output already fits.
pub(super) fn truncate_middle(output: &str, limit: usize) -> Option<String> {
    if output.len() <= limit {
        return None;
    }

    let head_budget = limit / TRUNCATION_HEAD_DIVISOR;
    let tail_budget = limit - head_budget;

    // Cut on line boundaries when there are any, so a single huge line (e.g.
    // minified JSON) still keeps a prefix and a suffix.
    let head_end = floor_char_boundary(output, head_budget);
    let head_end = output[..head_end].rfind('\n').unwrap_or(head_end);

    let tail_start = ceil_char_boundary(output, output.len() - tail_budget);
    let mut tail_start = output[tail_start..]
        .find('\n')
        .map_or(tail_start, |offset| tail_start + offset + 1);
    if tail_start < head_end {
        tail_start = head_end;
    }

    let head = &output[..head_end];
    let tail = &output[tail_start..];
    let omitted = &output[head_end..tail_start];
    let omitted_lines = omitted.lines().filter(|line| !line.is_empty()).count();

    let mut truncated = String::with_capacity(limit + 64);
    if !head.is_empty() {
        push_line(&mut truncated, head);
    }
    push_line(
        &mut truncated,
        &format!(
            "… [{omitted_lines} lines ({} bytes) omitted] …",
            omitted.len()
        ),
    );
    truncated.push('\n');
    truncated.push_str(tail);
    Some(truncated.trim_end().to_string())
}

fn push_line(text: &mut String, line: &str) {
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(line);
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn command_kinds(command: &str) -> CommandKinds {
    let mut kinds = CommandKinds::default();
    for segment in command.split(['\n', ';', '|', '&']) {
        let words = program_words(segment);
        let Some(program) = words.first() else {
            continue;
        };
        let program = program.rsplit('/').next().unwrap_or(program);
        let rest = &words[1..];
        match program {
            "cargo" => kinds.cargo = true,
            "jest" | "vitest" => kinds.javascript_tests = true,
            "npm" | "pnpm" | "yarn" | "bun" | "npx" | "bunx" => {
                if rest.iter().any(|word| {
                    matches!(*word, "test" | "jest" | "vitest") || word.starts_with("test:")
                }) {
                    kinds.javascript_tests = true;
                }
            }
            "pytest" => kinds.pytest = true,
            "python" | "python3" | "uv" | "poetry" => {
                if rest.contains(&"pytest") {
                    kinds.pytest = true;
                }
            }
            "go" => {
                if rest.first() == Some(&"test") {
                    kinds.go_test = true;
                }
            }
            "git" => {
                if git_subcommand(rest) == Some("status") {
                    kinds.git_status = true;
                }
            }
            _ => {}
        }
    }
    kinds
}

/// Splits a shell segment into words, skipping leading environment
/// assignments and transparent wrappers such as `env` or `time`.
fn program_words(segment: &str) -> Vec<&str> {
    let mut words = segment.split_whitespace().peekable();
    while let Some(word) = words.peek() {
        let is_assignment = word
            .split_once('=')
            .is_some_and(|(name, _)| !name.is_empty() && !name.starts_with('-'));
        let name = word.rsplit('/').next().unwrap_or(word);
        if is_assignment || matches!(name, "env" | "time" | "sudo" | "command" | "exec") {
            words.next();
        } else {
            break;
        }
    }
    words.collect()
}

fn git_subcommand<'a>(arguments: &[&'a str]) -> Option<&'a str> {
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        match *argument {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" => {
                arguments.next();
            }
            argument if argument.starts_with('-') => {}
            argument => return Some(argument),
        }
    }
    None
}

fn is_noise_line(line: &str, kinds: CommandKinds, following: &[&str]) -> bool {
    let trimmed = line.trim_start();
    (kinds.cargo && is_cargo_noise(trimmed, following))
        || (kinds.javascript_tests && is_javascript_test_noise(trimmed))
        || (kinds.pytest && is_pytest_noise(trimmed))
        || (kinds.go_test && is_go_test_noise(trimmed))
        || (kinds.git_status && is_git_status_noise(trimmed))
}

fn is_cargo_noise(line: &str, following: &[&str]) -> bool {
    const PROGRESS_PREFIXES: &[&str] = &[
        "Compiling ",
        "Checking ",
        "Downloaded ",
        "Downloading ",
        "Fresh ",
        "Blocking waiting for file lock",
        "Updating ",
        "Locking ",
        "Adding ",
        "Documenting ",
    ];
    if PROGRESS_PREFIXES
        .iter()
        .any(|prefix| line.starts_with(prefix))
    {
        return true;
    }
    if line.starts_with("test ") && (line.ends_with(" ... ok") || line.ends_with(" ... ignored")) {
        return true;
    }
    if line.starts_with("PASS [") {
        return true;
    }
    // Test binaries without any test print a three-line block (`Running ...`,
    // `running 0 tests`, an empty `test result`) that only adds noise.
    if line == "running 0 tests" || is_empty_test_result(line) {
        return true;
    }
    if line.starts_with("Running ") || line.starts_with("Doc-tests ") {
        return following
            .iter()
            .map(|line| line.trim())
            .find(|line| !line.is_empty())
            == Some("running 0 tests");
    }
    false
}

fn is_empty_test_result(line: &str) -> bool {
    line.starts_with("test result: ok. 0 passed; 0 failed; 0 ignored;")
}

fn is_javascript_test_noise(line: &str) -> bool {
    line.starts_with("✓ ")
        || line.starts_with("√ ")
        || line.starts_with("PASS ")
        || line.starts_with("(pass) ")
}

fn is_pytest_noise(line: &str) -> bool {
    line.contains("::") && (line.ends_with(" PASSED") || line.contains(" PASSED "))
}

fn is_go_test_noise(line: &str) -> bool {
    line.starts_with("=== RUN ")
        || line.starts_with("=== PAUSE ")
        || line.starts_with("=== CONT ")
        || line.starts_with("--- PASS: ")
        || (line.starts_with('?') && line.ends_with("[no test files]"))
}

fn is_git_status_noise(line: &str) -> bool {
    line.starts_with('(') && line.ends_with(')') && line.contains("\"git ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cargo_test_keeps_failures_and_drops_progress() {
        let output = r#"   Compiling foo v0.1.0 (/work/foo)
   Compiling bar v0.1.0 (/work/bar)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 3.21s
     Running unittests src/lib.rs (target/debug/deps/foo-1234)

running 3 tests
test tests::adds ... ok
test tests::ignored_one ... ignored
test tests::subtracts ... FAILED

failures:

---- tests::subtracts stdout ----
assertion failed: 1 == 2

test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out
     Running tests/empty.rs (target/debug/deps/empty-5678)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
"#;
        let compacted = compact_output("cargo test -p foo", output);

        assert_eq!(
            compacted.text,
            r#"    Finished `test` profile [unoptimized + debuginfo] target(s) in 3.21s
     Running unittests src/lib.rs (target/debug/deps/foo-1234)

running 3 tests
test tests::subtracts ... FAILED

failures:

---- tests::subtracts stdout ----
assertion failed: 1 == 2

test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out
"#
        );
        assert_eq!(compacted.omitted_lines, 8);
    }

    #[test]
    fn test_filters_only_apply_to_matching_commands() {
        let output = "   Compiling foo v0.1.0\ntest a ... ok";

        let compacted = compact_output("echo hi", output);

        assert_eq!(compacted.text, "   Compiling foo v0.1.0\ntest a ... ok");
        assert_eq!(compacted.omitted_lines, 0);
    }

    #[test]
    fn test_command_detection_skips_env_and_chained_commands() {
        assert!(command_kinds("RUST_LOG=info cargo test").cargo);
        assert!(command_kinds("cd foo && /usr/bin/env cargo build").cargo);
        assert!(command_kinds("pnpm run test:unit").javascript_tests);
        assert!(command_kinds("npx vitest run").javascript_tests);
        assert!(!command_kinds("npm install").javascript_tests);
        assert!(command_kinds("python -m pytest -q").pytest);
        assert!(command_kinds("go test ./...").go_test);
        assert!(!command_kinds("go build ./...").go_test);
        assert!(command_kinds("git --no-optional-locks status").git_status);
        assert!(command_kinds("git -C some/dir status --short").git_status);
        assert!(!command_kinds("git --no-pager log").git_status);
    }

    #[test]
    fn test_git_status_drops_hints() {
        let output = "\
On branch main
Changes not staged for commit:
  (use \"git add <file>...\" to update what will be committed)
  (use \"git restore <file>...\" to discard changes in working directory)
\tmodified:   src/main.rs

no changes added to commit (use \"git add\" and/or \"git commit -a\")";

        let compacted = compact_output("git --no-optional-locks status", output);

        assert_eq!(
            compacted.text,
            "\
On branch main
Changes not staged for commit:
\tmodified:   src/main.rs

no changes added to commit (use \"git add\" and/or \"git commit -a\")"
        );
    }

    #[test]
    fn test_javascript_pytest_and_go_passes_are_dropped() {
        let jest = compact_output(
            "npm test",
            "PASS src/a.test.ts\n  ✓ works (3 ms)\nFAIL src/b.test.ts\n  ✕ breaks (2 ms)",
        );
        assert_eq!(jest.text, "FAIL src/b.test.ts\n  ✕ breaks (2 ms)");

        let pytest = compact_output(
            "pytest -v",
            "tests/test_a.py::test_one PASSED    [ 50%]\ntests/test_a.py::test_two FAILED    [100%]",
        );
        assert_eq!(pytest.text, "tests/test_a.py::test_two FAILED    [100%]");

        let go = compact_output(
            "go test -v ./...",
            "=== RUN   TestA\n--- PASS: TestA (0.00s)\n=== RUN   TestB\n--- FAIL: TestB (0.00s)\n?   \texample.com/cmd\t[no test files]\nFAIL",
        );
        assert_eq!(go.text, "--- FAIL: TestB (0.00s)\nFAIL");
    }

    #[test]
    fn test_repeated_lines_and_blank_runs_are_collapsed() {
        let output = "start\nwarning: retrying\nwarning: retrying\nwarning: retrying\nwarning: retrying\n\n\n\nend\nsame\nsame";

        let compacted = compact_output("some-tool", output);

        assert_eq!(
            compacted.text,
            "start\nwarning: retrying\n… (previous line repeated 3 more times)\n\nend\nsame\nsame"
        );
        assert_eq!(compacted.omitted_lines, 5);
    }

    #[test]
    fn test_truncate_middle_keeps_head_and_tail_on_line_boundaries() {
        let output = (0..100)
            .map(|index| format!("line {index:03}"))
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(truncate_middle(&output, output.len()), None);

        let truncated = truncate_middle(&output, 200).expect("output should be truncated");
        let lines = truncated.lines().collect::<Vec<_>>();

        assert_eq!(lines.first(), Some(&"line 000"));
        assert_eq!(lines.last(), Some(&"line 099"));
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("… [") && line.ends_with("omitted] …")),
            "expected an omission marker, got: {truncated}"
        );
        assert!(
            lines
                .iter()
                .all(|line| line.starts_with("line ") || line.starts_with("… ["))
        );
        assert!(truncated.len() < 300);
    }

    #[test]
    fn test_truncate_middle_handles_multibyte_text() {
        let output = "é".repeat(500);

        let truncated = truncate_middle(&output, 100).expect("output should be truncated");

        assert!(truncated.starts_with('é'));
        assert!(truncated.ends_with('é'));
        assert!(truncated.contains("omitted"));
        assert!(truncated.len() < 200);
    }
}
