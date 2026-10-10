//! The unified-diff renderer behind `apply --dry-run` (internal; no diff
//! dependency).
//!
//! [`render_unified_diff`] needs no LCS: the changed line ranges are
//! derived directly from the edit list, so hunks are exact by
//! construction. Each edit's byte range `[s, e)` in the original, and its
//! image `[s', e')` in the edited text, is widened to whole lines:
//!
//! - the start moves back to the start of its line (the bytes skipped are
//!   unchanged, so the same distance back from `s'` is a line start in the
//!   edited text);
//! - the end stays put when `e` and `e'` are *both* line starts (the edit
//!   replaced whole lines with whole lines); otherwise it moves forward
//!   past the next line terminator, or to the end of the text, by the same
//!   distance on both sides (the bytes skipped are unchanged).
//!
//! Widened ranges that overlap (two edits touching one line) or abut are
//! merged into one change, so a change lists all its removed lines before
//! all its added ones. Each change is then trimmed of the lines its two
//! sides share at either end, so an edit rewriting text to itself renders
//! nothing. Between changes the original and edited lines are
//! byte-identical, so each change maps an original line range onto an
//! edited one. Changes become hunks with 3 lines of context; two
//! changes whose contexts would overlap or touch share a hunk.
//!
//! Lines are emitted verbatim, so the `\r` of a CRLF line stays inside its
//! line; a final line without a terminator is followed by the
//! `\ No newline at end of file` marker. The output is valid input for
//! `git apply`.

use super::edits::application_order;
use crate::comments::types::Edit;

/// Lines of context around each change.
const CONTEXT: usize = 3;

/// The marker following a line that has no terminator.
const NO_NEWLINE_MARKER: &str = "\\ No newline at end of file\n";

/// Renders the unified diff turning `original` into `edited` for the
/// artifact-relative `path`, or `""` when there is no change.
///
/// `edits` are byte-offset edits against `original` satisfying the edit
/// invariants of [`super::edits`] (in any order), and `edited` must be
/// their result, i.e. `apply_edits(original, edits)`. An edit that
/// replaces nothing with nothing is ignored.
///
/// # Panics
///
/// If an edit is out of bounds of `original` or off a `char` boundary;
/// both are rejected by `apply_edits`, which produced `edited`.
pub fn render_unified_diff(path: &str, original: &str, edited: &str, edits: &[Edit]) -> String {
    let old_lines = Lines::new(original);
    let new_lines = Lines::new(edited);
    let changes = line_changes(original, edited, edits, &old_lines, &new_lines);
    if changes.is_empty() {
        return String::new();
    }
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    for group in hunk_groups(&changes) {
        render_hunk(&mut out, group, &old_lines, &new_lines);
    }
    out
}

/// One changed region: original lines `[old.0, old.1)` became edited
/// lines `[new.0, new.1)` (0-based, half-open line indices).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Change {
    old: (usize, usize),
    new: (usize, usize),
}

/// A changed region in bytes: original `[old.0, old.1)` became edited
/// `[new.0, new.1)`, both widened to line boundaries.
#[derive(Debug, Clone, Copy)]
struct ByteChange {
    old: (usize, usize),
    new: (usize, usize),
}

/// The line-aligned changes `edits` make, in order, non-overlapping, each
/// trimmed of the lines its two sides share at either end (so an edit that
/// rewrites text to itself is no change).
fn line_changes(
    original: &str,
    edited: &str,
    edits: &[Edit],
    old_lines: &Lines,
    new_lines: &Lines,
) -> Vec<Change> {
    let mut ordered: Vec<&Edit> = edits
        .iter()
        .filter(|edit| edit.length != 0 || !edit.replacement.is_empty())
        .collect();
    ordered.sort_by(|a, b| application_order(a, b));

    let mut merged: Vec<ByteChange> = Vec::new();
    // The end of the previous edit in the original and in the edited text:
    // the bytes between it and the next edit's start are unchanged.
    let (mut old_cursor, mut new_cursor) = (0, 0);
    for edit in ordered {
        let (s, e) = (edit.start, edit.start + edit.length);
        let s_new = new_cursor + (s - old_cursor);
        let e_new = s_new + edit.replacement.len();
        (old_cursor, new_cursor) = (e, e_new);

        let line_start = line_start_at(original, s);
        let line_end = if is_line_start(original, e) && is_line_start(edited, e_new) {
            e
        } else {
            line_end_at(original, e)
        };
        match merged.last_mut() {
            // The widened range reaches into, or abuts, the previous
            // change's lines: one change, starting where the previous one
            // starts (so its removed lines precede all its added ones).
            Some(prev) if line_start <= prev.old.1 => {
                let end = prev.old.1.max(line_end);
                prev.old.1 = end;
                prev.new.1 = e_new + (end - e);
            }
            _ => merged.push(ByteChange {
                old: (line_start, line_end),
                new: (s_new - (s - line_start), e_new + (line_end - e)),
            }),
        }
    }

    merged
        .into_iter()
        .filter_map(|change| {
            let mut old = (
                old_lines.index_at(change.old.0),
                old_lines.index_at(change.old.1),
            );
            let mut new = (
                new_lines.index_at(change.new.0),
                new_lines.index_at(change.new.1),
            );
            while old.0 < old.1 && new.0 < new.1 && old_lines.lines[old.0] == new_lines.lines[new.0]
            {
                old.0 += 1;
                new.0 += 1;
            }
            while old.0 < old.1
                && new.0 < new.1
                && old_lines.lines[old.1 - 1] == new_lines.lines[new.1 - 1]
            {
                old.1 -= 1;
                new.1 -= 1;
            }
            (old.0 < old.1 || new.0 < new.1).then_some(Change { old, new })
        })
        .collect()
}

/// Whether byte `pos` of `text` starts a line: offset 0, or just after a
/// `\n` (the end of a text ending in `\n` included).
fn is_line_start(text: &str, pos: usize) -> bool {
    pos == 0 || text.as_bytes()[pos - 1] == b'\n'
}

/// The start of the line containing byte `pos` of `text`.
fn line_start_at(text: &str, pos: usize) -> usize {
    text[..pos].rfind('\n').map_or(0, |nl| nl + 1)
}

/// Just past the first `\n` at or after byte `pos` of `text`, or the end of
/// the text when there is none.
fn line_end_at(text: &str, pos: usize) -> usize {
    text[pos..].find('\n').map_or(text.len(), |nl| pos + nl + 1)
}

/// A text split into lines, each keeping its terminator.
struct Lines<'a> {
    /// The lines, in order (`split_inclusive('\n')`).
    lines: Vec<&'a str>,
    /// The byte offset each line starts at.
    starts: Vec<usize>,
}

impl<'a> Lines<'a> {
    fn new(text: &'a str) -> Self {
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let starts = lines
            .iter()
            .scan(0, |offset, line| {
                let start = *offset;
                *offset += line.len();
                Some(start)
            })
            .collect();
        Self { lines, starts }
    }

    /// The index of the line starting at byte `pos`, or the line count when
    /// `pos` is the end of the text (`pos` must be one or the other).
    fn index_at(&self, pos: usize) -> usize {
        self.starts.partition_point(|&start| start < pos)
    }

    fn len(&self) -> usize {
        self.lines.len()
    }
}

/// Splits `changes` into hunk groups: consecutive changes whose context
/// windows overlap or touch (at most `2 * CONTEXT` unchanged lines apart)
/// share a hunk.
fn hunk_groups(changes: &[Change]) -> impl Iterator<Item = &[Change]> {
    changes.chunk_by(|a, b| b.old.0 - a.old.1 <= 2 * CONTEXT)
}

/// Appends the hunk covering `group` (non-empty) to `out`.
fn render_hunk(out: &mut String, group: &[Change], old: &Lines, new: &Lines) {
    let (first, last) = (group[0], group[group.len() - 1]);
    // Unchanged lines are identical on both sides, so the leading and the
    // trailing context have the same length in both texts.
    let lead = first.old.0.min(CONTEXT);
    let trail = (old.len() - last.old.1).min(CONTEXT);
    let old_range = (first.old.0 - lead, last.old.1 + trail);
    let new_range = (first.new.0 - lead, last.new.1 + trail);
    out.push_str(&format!(
        "@@ -{} +{} @@\n",
        hunk_range(old_range),
        hunk_range(new_range)
    ));

    let mut old_cursor = old_range.0;
    for change in group {
        push_lines(out, ' ', &old.lines[old_cursor..change.old.0]);
        push_lines(out, '-', &old.lines[change.old.0..change.old.1]);
        push_lines(out, '+', &new.lines[change.new.0..change.new.1]);
        old_cursor = change.old.1;
    }
    push_lines(out, ' ', &old.lines[old_cursor..old_range.1]);
}

/// A hunk header range `start,count` for the 0-based half-open line range:
/// `start` is 1-based, or, for an empty range, the line before it (0 at
/// the top of the file).
fn hunk_range((start, end): (usize, usize)) -> String {
    let count = end - start;
    let first = if count == 0 { start } else { start + 1 };
    format!("{first},{count}")
}

/// Appends each of `lines` behind `prefix`, verbatim; a line without a
/// terminator (the last line of its text) gets the no-newline marker.
fn push_lines(out: &mut String, prefix: char, lines: &[&str]) {
    for line in lines {
        out.push(prefix);
        out.push_str(line);
        if !line.ends_with('\n') {
            out.push('\n');
            out.push_str(NO_NEWLINE_MARKER);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use super::*;
    use crate::apply::edits::apply_edits;

    fn edit(start: usize, length: usize, replacement: &str) -> Edit {
        Edit {
            start,
            length,
            replacement: replacement.to_owned(),
        }
    }

    /// An edit replacing the first occurrence of `needle` in `text`.
    fn replace(text: &str, needle: &str, replacement: &str) -> Edit {
        let start = text.find(needle).expect("the needle occurs in the text");
        edit(start, needle.len(), replacement)
    }

    /// An insertion of `inserted` just before the first `before` in `text`.
    fn insert(text: &str, before: &str, inserted: &str) -> Edit {
        let start = text.find(before).expect("the anchor occurs in the text");
        edit(start, 0, inserted)
    }

    /// Renders the diff of `edits` on `original`, asserts it is `expected`,
    /// and checks with `git apply` that it reproduces the edited text.
    fn check(path: &str, original: &str, edits: &[Edit], expected: &str) {
        let edited = apply_edits(original, edits).expect("the test edits are valid");
        let diff = render_unified_diff(path, original, &edited, edits);
        assert_eq!(diff, expected, "diff of {original:?} -> {edited:?}");
        git_apply_roundtrip(&[(path.to_owned(), original.to_owned(), edited)], &diff);
    }

    /// Whether a usable `git` is on the `PATH`.
    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    }

    /// Runs `git apply <args> <patch>` in `dir`, isolated from the user's
    /// and the system's git configuration and from any enclosing
    /// repository, and panics with git's stderr on failure.
    fn run_git_apply(dir: &Path, args: &[&str], patch: &Path) {
        let parent = dir.parent().expect("a temp dir has a parent");
        let out = Command::new("git")
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CEILING_DIRECTORIES", parent)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .arg("apply")
            .args(args)
            .arg(patch)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git apply {args:?} failed:\n{}\npatch:\n{}",
            String::from_utf8_lossy(&out.stderr),
            fs::read_to_string(patch).unwrap_or_default()
        );
    }

    /// Writes each `(path, original, _)` into a fresh temp dir, checks
    /// `diff` with `git apply --check`, applies it with `git apply`, and
    /// asserts every file then holds its `edited` text byte-for-byte.
    /// Skipped, with a note, when `git` is unavailable.
    fn git_apply_roundtrip(files: &[(String, String, String)], diff: &str) {
        if !git_available() {
            eprintln!("note: git is unavailable; skipping the `git apply` check");
            return;
        }
        let dir = tempfile::tempdir().expect("tests may create temp directories");
        for (path, original, _) in files {
            let file = dir.path().join(path);
            fs::create_dir_all(file.parent().expect("a file has a parent"))
                .expect("create the parent directory");
            fs::write(&file, original).expect("write the original");
        }
        let patch_dir = tempfile::tempdir().expect("tests may create temp directories");
        let patch = patch_dir.path().join("change.patch");
        fs::write(&patch, diff).expect("write the patch");

        run_git_apply(dir.path(), &["--check"], &patch);
        run_git_apply(dir.path(), &[], &patch);
        for (path, _, edited) in files {
            let applied = fs::read(dir.path().join(path)).expect("read the patched file");
            assert_eq!(
                String::from_utf8_lossy(&applied),
                edited.as_str(),
                "{path}: git apply did not reproduce the edited text"
            );
            assert_eq!(applied, edited.as_bytes());
        }
    }

    #[test]
    fn no_edits_render_nothing() {
        assert_eq!(render_unified_diff("a.rs", "a\n", "a\n", &[]), "");
        // An edit replacing nothing with nothing is no change.
        assert_eq!(
            render_unified_diff("a.rs", "a\n", "a\n", &[edit(1, 0, "")]),
            ""
        );
    }

    #[test]
    fn single_line_removal_keeps_three_lines_of_context() {
        let original = "a\nb\nc\nd\ne // c\nf\ng\nh\ni\nj\n";
        check(
            "src/x.rs",
            original,
            &[replace(original, " // c", "")],
            "--- a/src/x.rs\n+++ b/src/x.rs\n\
             @@ -2,7 +2,7 @@\n b\n c\n d\n-e // c\n+e\n f\n g\n h\n",
        );
    }

    #[test]
    fn whole_line_removal() {
        let original = "a\nb\nc\nd\n// c\ne\nf\ng\nh\n";
        check(
            "x.rs",
            original,
            &[replace(original, "// c\n", "")],
            "--- a/x.rs\n+++ b/x.rs\n\
             @@ -2,7 +2,6 @@\n b\n c\n d\n-// c\n e\n f\n g\n",
        );
    }

    #[test]
    fn multi_line_replacement() {
        let original = "a\n/* one\n   two */\nb\n";
        check(
            "x.c",
            original,
            &[replace(
                original,
                "/* one\n   two */",
                "/* uno\n * dos\n * tres */",
            )],
            "--- a/x.c\n+++ b/x.c\n\
             @@ -1,4 +1,5 @@\n a\n-/* one\n-   two */\n+/* uno\n+ * dos\n+ * tres */\n b\n",
        );
    }

    #[test]
    fn new_insertion_adds_a_line() {
        let original = "fn f() {\n    x();\n}\n";
        check(
            "x.rs",
            original,
            &[insert(original, "    x();", "    // new\n")],
            "--- a/x.rs\n+++ b/x.rs\n\
             @@ -1,3 +1,4 @@\n fn f() {\n+    // new\n     x();\n }\n",
        );
    }

    /// `"1\n2\n…\n{n}\n"`.
    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("{i}\n")).collect()
    }

    #[test]
    fn nearby_edits_share_a_hunk() {
        let original = numbered(14);
        // Five unchanged lines apart: the context windows overlap.
        check(
            "n.txt",
            &original,
            &[
                replace(&original, "3\n", "three\n"),
                replace(&original, "9\n", "nine\n"),
            ],
            "--- a/n.txt\n+++ b/n.txt\n\
             @@ -1,12 +1,12 @@\n 1\n 2\n-3\n+three\n 4\n 5\n 6\n 7\n 8\n-9\n+nine\n 10\n 11\n 12\n",
        );
        // Six apart: the context windows touch, which still merges.
        check(
            "n.txt",
            &original,
            &[
                replace(&original, "3\n", "three\n"),
                replace(&original, "10\n", "ten\n"),
            ],
            "--- a/n.txt\n+++ b/n.txt\n\
             @@ -1,13 +1,13 @@\n 1\n 2\n-3\n+three\n 4\n 5\n 6\n 7\n 8\n 9\n-10\n+ten\n 11\n 12\n 13\n",
        );
    }

    #[test]
    fn distant_edits_get_separate_hunks() {
        let original = numbered(14);
        // Seven unchanged lines apart: one line between the windows.
        check(
            "n.txt",
            &original,
            &[
                replace(&original, "3\n", "three\n"),
                replace(&original, "11\n", "eleven\n"),
            ],
            "--- a/n.txt\n+++ b/n.txt\n\
             @@ -1,6 +1,6 @@\n 1\n 2\n-3\n+three\n 4\n 5\n 6\n\
             @@ -8,7 +8,7 @@\n 8\n 9\n 10\n-11\n+eleven\n 12\n 13\n 14\n",
        );
    }

    #[test]
    fn edits_on_one_line_merge() {
        let original = "a\nx = 1; /* a */ y = 2; /* b */\nb\n";
        check(
            "x.c",
            original,
            &[
                replace(original, " /* a */", ""),
                replace(original, " /* b */", ""),
            ],
            "--- a/x.c\n+++ b/x.c\n\
             @@ -1,3 +1,3 @@\n a\n-x = 1; /* a */ y = 2; /* b */\n+x = 1; y = 2;\n b\n",
        );
    }

    #[test]
    fn edits_at_line_one_and_at_eof() {
        let original = format!("// head\n{}// tail\n", numbered(10));
        check(
            "x.rs",
            &original,
            &[
                replace(&original, "// head\n", ""),
                replace(&original, "// tail", "// TAIL"),
            ],
            "--- a/x.rs\n+++ b/x.rs\n\
             @@ -1,4 +1,3 @@\n-// head\n 1\n 2\n 3\n\
             @@ -9,4 +8,4 @@\n 8\n 9\n 10\n-// tail\n+// TAIL\n",
        );
    }

    #[test]
    fn insertions_at_the_top_of_a_file() {
        check(
            "x.py",
            "a\nb\n",
            &[edit(0, 0, "# new\n")],
            "--- a/x.py\n+++ b/x.py\n@@ -1,2 +1,3 @@\n+# new\n a\n b\n",
        );
        // An empty file: the old range is empty and starts at line 0.
        check(
            "x.py",
            "",
            &[edit(0, 0, "# new\n")],
            "--- a/x.py\n+++ b/x.py\n@@ -0,0 +1,1 @@\n+# new\n",
        );
    }

    #[test]
    fn a_file_without_a_trailing_newline() {
        let original = "a\nb\nc // c";
        check(
            "x.js",
            original,
            &[replace(original, " // c", "")],
            "--- a/x.js\n+++ b/x.js\n\
             @@ -1,3 +1,3 @@\n a\n b\n-c // c\n\\ No newline at end of file\n\
             +c\n\\ No newline at end of file\n",
        );
        // Removing the unterminated last line leaves the line before it
        // unterminated.
        let original = "a\nb\n// c";
        check(
            "x.js",
            original,
            &[replace(original, "\n// c", "")],
            "--- a/x.js\n+++ b/x.js\n\
             @@ -1,3 +1,2 @@\n a\n-b\n-// c\n\\ No newline at end of file\n\
             +b\n\\ No newline at end of file\n",
        );
        // An unterminated line as trailing context.
        let original = "a // c\nb";
        check(
            "x.js",
            original,
            &[replace(original, " // c", "")],
            "--- a/x.js\n+++ b/x.js\n\
             @@ -1,2 +1,2 @@\n-a // c\n+a\n b\n\\ No newline at end of file\n",
        );
    }

    #[test]
    fn a_crlf_file_keeps_its_carriage_returns() {
        let original = "a\r\nb // c\r\n// d\r\nc\r\n";
        check(
            "x.py",
            original,
            &[
                replace(original, " // c", ""),
                replace(original, "// d\r\n", ""),
            ],
            "--- a/x.py\n+++ b/x.py\n\
             @@ -1,4 +1,3 @@\n a\r\n-b // c\r\n-// d\r\n+b\r\n c\r\n",
        );
    }

    #[test]
    fn an_insertion_before_a_removal_at_the_same_offset() {
        let original = "a\n// c\nb\n";
        let start = original.find("// c").expect("present");
        check(
            "x.rs",
            original,
            // Out of application order: the renderer orders them itself.
            &[edit(start, 5, ""), edit(start, 0, "// n\n")],
            "--- a/x.rs\n+++ b/x.rs\n@@ -1,3 +1,3 @@\n a\n-// c\n+// n\n b\n",
        );
    }

    /// Every valid edit list of up to two edits over small texts (LF, CRLF,
    /// unterminated, empty, blank-line-only). A diff is never empty for a
    /// changed text, and a single edit that changes nothing renders
    /// nothing. (Two edits may cancel each other out in different places:
    /// their diff shows both, as the edit list is the source of truth.)
    /// All the diffs, applied as one multi-file patch by `git apply`, must
    /// reproduce every edited text.
    #[test]
    fn every_small_edit_list_round_trips_through_git_apply() {
        const TEXTS: &[&str] = &["a\nb\nc\n", "a\nbc", "a\r\nb\r\n", "\n\n", "", "x"];
        const REPLACEMENTS: &[&str] = &["", "y", "\n", "y\n", "y\nz"];
        let mut files = Vec::new();
        let mut patch = String::new();
        for (t, text) in TEXTS.iter().enumerate() {
            let singles: Vec<Edit> = (0..=text.len())
                .flat_map(|start| (start..=text.len()).map(move |end| (start, end)))
                .flat_map(|(start, end)| {
                    REPLACEMENTS
                        .iter()
                        .map(move |r| edit(start, end - start, r))
                })
                .collect();
            let pairs = singles
                .iter()
                .enumerate()
                .flat_map(|(i, a)| singles[i..].iter().map(move |b| vec![a.clone(), b.clone()]));
            let lists = singles.iter().map(|e| vec![e.clone()]).chain(pairs);
            for edits in lists {
                let Ok(edited) = apply_edits(text, &edits) else {
                    continue;
                };
                let path = format!("t{t}/case{}.txt", files.len());
                let diff = render_unified_diff(&path, text, &edited, &edits);
                let context = format!("{text:?} with {edits:?} -> {edited:?}:\n{diff}");
                if edited != *text {
                    assert!(!diff.is_empty(), "{context}");
                } else if edits.len() == 1 {
                    assert!(diff.is_empty(), "{context}");
                }
                patch.push_str(&diff);
                files.push((path, text.to_string(), edited));
            }
        }
        git_apply_roundtrip(&files, &patch);
    }
}
