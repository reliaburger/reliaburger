//! `relish agent patch apply`: apply a unified diff to the workspace,
//! trusting its lines and not its hunk headers' line counts (#1691).
//!
//! An agent that writes a diff by hand gets the changed lines right and the
//! `@@ -12,4 +12,5 @@` counts wrong, and `git apply` then calls the whole
//! patch corrupt. Here the counts are only a hint: each hunk's body says
//! which lines it removes and keeps, and the hunk applies where those lines
//! sit in the file, nearest the line its header gave. `patch diff` then
//! prints the change with headers git wrote itself.
//!
//! It's all or nothing: every hunk is matched in memory first, and a hunk
//! that matches nowhere refuses the whole diff with a usage error naming it.
//! Only files inside the workspace are written, never `.git/` or `patch`'s
//! own state, and never through a symbolic link.

use std::path::{Component, Path, PathBuf};

use schemars::JsonSchema;
use serde::Serialize;

use super::workspace::{FileStatus, State};
use super::{PATCH_DIR, answer_of, patch_command, shown, step};
use crate::relish::agent::envelope::{AgentError, ErrorCode};
use crate::relish::agent::source::Answer;

/// The largest diff `apply` reads.
const MAX_DIFF_BYTES: usize = 1024 * 1024;

/// `data` for `relish agent patch apply`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ApplyData {
    /// The workspace.
    pub dir: String,
    /// `true` with `--check`: every hunk was matched but nothing was written.
    pub checked_only: bool,
    /// Each file the diff touches, in the order the diff names them.
    pub files: Vec<AppliedFile>,
}

/// One file the diff touches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct AppliedFile {
    /// Workspace-relative path.
    pub path: String,
    /// Whether the diff adds, changes or deletes it.
    pub status: FileStatus,
    /// Its hunks, in order.
    pub hunks: Vec<AppliedHunk>,
}

/// One hunk, and where it applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct AppliedHunk {
    /// The hunk's number within its file, from 1.
    pub hunk: usize,
    /// The `@@ ... @@` line as the diff wrote it.
    pub header: String,
    /// The first line of the original file the hunk covers, from 1; for a
    /// hunk that only adds lines, the line they go after (0 for the top).
    pub line: u64,
    /// `line` minus the line the header gave: 0 when the header was right.
    pub offset: i64,
    /// Lines the hunk removes, counted from its body.
    pub removed: u64,
    /// Lines the hunk adds, counted from its body.
    pub added: u64,
    /// `true` when the header's line counts didn't match the body. The
    /// hunk still applied; `patch diff` prints it with the right ones.
    pub recounted: bool,
}

fn usage(message: impl Into<String>) -> AgentError {
    AgentError::new(ErrorCode::Usage, message)
}

/// The diff argument: the text itself, or the file it names after `@`.
pub(super) fn read_diff(value: &str) -> Result<String, AgentError> {
    let text = match value.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|error| usage(format!("couldn't read the diff from {path}: {error}")))?,
        None => value.to_string(),
    };
    if text.len() > MAX_DIFF_BYTES {
        return Err(usage(format!(
            "the diff is {} bytes; at most {MAX_DIFF_BYTES}",
            text.len()
        )));
    }
    Ok(text)
}

/// One line of a hunk's body.
#[derive(Debug)]
enum Line<'a> {
    Context(&'a str),
    Removed(&'a str),
    Added(&'a str),
}

/// One `@@` hunk as the diff wrote it.
#[derive(Debug)]
struct Hunk<'a> {
    header: &'a str,
    /// The header's old start line.
    old_start: usize,
    /// The header's line counts, which may be wrong.
    old_count: usize,
    new_count: usize,
    lines: Vec<Line<'a>>,
    /// `\ No newline at end of file` followed the old side's last line.
    old_lacks_newline: bool,
    /// The same, for the new side.
    new_lacks_newline: bool,
}

impl Hunk<'_> {
    /// The lines the file has now: context and removed.
    fn old_lines(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter_map(|line| match line {
                Line::Context(text) | Line::Removed(text) => Some(*text),
                Line::Added(_) => None,
            })
            .collect()
    }

    /// The lines it will have: context and added.
    fn new_lines(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter_map(|line| match line {
                Line::Context(text) | Line::Added(text) => Some(*text),
                Line::Removed(_) => None,
            })
            .collect()
    }
}

/// One file's `---`/`+++` pair and its hunks. `None` is `/dev/null`.
#[derive(Debug)]
struct FilePatch<'a> {
    old: Option<String>,
    new: Option<String>,
    hunks: Vec<Hunk<'a>>,
}

/// A `--- ` line followed by a `+++ ` line starts a file.
fn is_file_header(lines: &[&str], index: usize) -> bool {
    lines[index].starts_with("--- ")
        && lines
            .get(index + 1)
            .is_some_and(|next| next.starts_with("+++ "))
}

/// The path a `---` or `+++` line names, without git's `a/` or `b/`.
fn header_path(raw: &str) -> Result<Option<String>, AgentError> {
    // Some tools add a tab and a timestamp after the path.
    let path = raw.split('\t').next().unwrap_or(raw).trim_end();
    if path == "/dev/null" {
        return Ok(None);
    }
    if path.starts_with('"') {
        return Err(usage(format!(
            "quoted path {path} isn't supported; name the file unquoted"
        )));
    }
    let path = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    if path.is_empty() {
        return Err(usage(format!("{raw:?} names no file")));
    }
    Ok(Some(path.to_string()))
}

/// `-12,4` or `-12`: a start line and a count, which defaults to 1.
fn range(text: &str) -> Option<(usize, usize)> {
    match text.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((text.parse().ok()?, 1)),
    }
}

/// `@@ -12,4 +12,5 @@ anything`: the old start and both counts.
fn parse_header(header: &str) -> Option<(usize, usize, usize)> {
    let rest = header.strip_prefix("@@ -")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let (old_start, old_count) = range(old)?;
    let (_, new_count) = range(new)?;
    Some((old_start, old_count, new_count))
}

/// The hunk whose header is `lines[start]`, and the index after its body.
///
/// The body runs until the next hunk or file, or a line that can't be part
/// of a hunk, whatever the header's counts say. An empty line is an empty
/// context line whose leading space was lost, unless it trails the hunk.
fn parse_hunk<'a>(lines: &[&'a str], start: usize) -> Result<(Hunk<'a>, usize), AgentError> {
    let header = lines[start];
    let (old_start, old_count, new_count) = parse_header(header).ok_or_else(|| {
        usage(format!(
            "{header:?} isn't a hunk header like \"@@ -12,4 +12,5 @@\""
        ))
    })?;
    let mut hunk = Hunk {
        header,
        old_start,
        old_count,
        new_count,
        lines: Vec::new(),
        old_lacks_newline: false,
        new_lacks_newline: false,
    };
    let mut trailing_empty = 0;
    let mut index = start + 1;
    while index < lines.len() {
        let line = lines[index];
        if line.starts_with("@@") || line.starts_with("diff --git ") || is_file_header(lines, index)
        {
            break;
        }
        match line.chars().next() {
            None => hunk.lines.push(Line::Context("")),
            Some(' ') => hunk.lines.push(Line::Context(&line[1..])),
            Some('-') => hunk.lines.push(Line::Removed(&line[1..])),
            Some('+') => hunk.lines.push(Line::Added(&line[1..])),
            Some('\\') => match hunk.lines.last() {
                Some(Line::Removed(_)) => hunk.old_lacks_newline = true,
                Some(Line::Added(_)) => hunk.new_lacks_newline = true,
                _ => {
                    hunk.old_lacks_newline = true;
                    hunk.new_lacks_newline = true;
                }
            },
            Some(_) => break,
        }
        trailing_empty = if line.is_empty() {
            trailing_empty + 1
        } else {
            0
        };
        index += 1;
    }
    hunk.lines.truncate(hunk.lines.len() - trailing_empty);
    Ok((hunk, index))
}

/// Every file in a unified diff, with its hunks.
fn parse(text: &str) -> Result<Vec<FilePatch<'_>>, AgentError> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let mut files: Vec<FilePatch> = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if is_file_header(&lines, index) {
            files.push(FilePatch {
                old: header_path(&line[4..])?,
                new: header_path(&lines[index + 1][4..])?,
                hunks: Vec::new(),
            });
            index += 2;
        } else if line.starts_with("@@") {
            let Some(file) = files.last_mut() else {
                return Err(usage(
                    "a hunk comes before any --- and +++ lines naming its file",
                ));
            };
            let (hunk, next) = parse_hunk(&lines, index)?;
            file.hunks.push(hunk);
            index = next;
        } else if line.starts_with("GIT binary patch") || line.starts_with("Binary files ") {
            return Err(usage("binary diffs aren't supported"));
        } else {
            // `diff --git`, `index`, mode lines and any prose around the diff.
            index += 1;
        }
    }
    if files.is_empty() {
        return Err(usage(
            "no --- and +++ lines naming a file: this isn't a unified diff",
        ));
    }
    Ok(files)
}

/// A text file as lines, without their `\n`.
#[derive(Debug)]
struct Text {
    lines: Vec<String>,
    final_newline: bool,
}

impl Text {
    fn parse(content: &str) -> Self {
        let mut lines: Vec<String> = content.split('\n').map(str::to_string).collect();
        let final_newline = lines.last().is_some_and(String::is_empty);
        if final_newline {
            lines.pop();
        }
        Text {
            lines,
            final_newline,
        }
    }

    fn render(&self) -> String {
        let mut text = self.lines.join("\n");
        if self.final_newline && !self.lines.is_empty() {
            text.push('\n');
        }
        text
    }
}

/// Where `old` sits in `lines`, at or after `from`, nearest `expected`.
fn find(lines: &[String], old: &[&str], expected: usize, from: usize) -> Option<usize> {
    if old.len() > lines.len() {
        return None;
    }
    (from..=lines.len() - old.len())
        .filter(|&at| {
            lines[at..at + old.len()]
                .iter()
                .zip(old)
                .all(|(a, b)| a == b)
        })
        .min_by_key(|&at| at.abs_diff(expected))
}

/// Why `old` isn't in `lines`: the first of its lines the file lacks, or
/// that they're all there but not together.
fn why_not(lines: &[String], old: &[&str]) -> String {
    match old
        .iter()
        .find(|want| !lines.iter().any(|line| line == *want))
    {
        Some(missing) => format!("the file has no line {missing:?}"),
        None => "its context and removed lines are in the file, but not together in this order"
            .to_string(),
    }
}

/// Apply `hunks`, in order, to `text`.
fn apply_hunks(
    path: &str,
    text: &mut Text,
    hunks: &[Hunk],
) -> Result<Vec<AppliedHunk>, AgentError> {
    let mut reports = Vec::new();
    // How far the hunks so far moved the lines after them.
    let mut delta: i64 = 0;
    let mut from = 0;
    for (number, hunk) in hunks.iter().enumerate() {
        let old = hunk.old_lines();
        let new = hunk.new_lines();
        // A hunk that only adds says the line it goes after; any other says
        // its first line.
        let stated = if old.is_empty() {
            hunk.old_start
        } else {
            hunk.old_start.saturating_sub(1)
        };
        let expected = usize::try_from(stated as i64 + delta).unwrap_or(0);
        let at = if old.is_empty() {
            expected.clamp(from, text.lines.len())
        } else {
            find(&text.lines, &old, expected, from).ok_or_else(|| {
                usage(format!(
                    "hunk {} of {path} ({}) doesn't apply: {}",
                    number + 1,
                    hunk.header,
                    why_not(&text.lines, &old)
                ))
                .with_hint(
                    "nothing was written; copy the context and - lines exactly from the file in \
                     the workspace (the header's line counts don't matter) and apply again",
                )
            })?
        };
        let original = at as i64 - delta;
        let line = original + i64::from(!old.is_empty());
        text.lines
            .splice(at..at + old.len(), new.iter().map(|line| line.to_string()));
        if at + new.len() == text.lines.len() {
            if hunk.new_lacks_newline {
                text.final_newline = false;
            } else if hunk.old_lacks_newline {
                text.final_newline = true;
            }
        }
        from = at + new.len();
        delta += new.len() as i64 - old.len() as i64;
        reports.push(AppliedHunk {
            hunk: number + 1,
            header: hunk.header.to_string(),
            line: u64::try_from(line).unwrap_or(0),
            offset: line - hunk.old_start as i64,
            removed: (old.len() - count_context(hunk)) as u64,
            added: (new.len() - count_context(hunk)) as u64,
            recounted: hunk.old_count != old.len() || hunk.new_count != new.len(),
        });
    }
    Ok(reports)
}

fn count_context(hunk: &Hunk) -> usize {
    hunk.lines
        .iter()
        .filter(|line| matches!(line, Line::Context(_)))
        .count()
}

/// `path` inside the workspace `dir`, or why the diff may not write it:
/// it leaves the workspace, it's in `.git/` or `patch`'s state, or it goes
/// through a symbolic link, which could point anywhere.
fn workspace_path(dir: &Path, path: &str) -> Result<PathBuf, AgentError> {
    let relative = Path::new(path);
    if !relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(usage(format!(
            "{path} is outside the workspace; name files relative to it"
        )));
    }
    if let Some(Component::Normal(first)) = relative.components().next()
        && (first == ".git" || first == PATCH_DIR)
    {
        return Err(usage(format!(
            "{path} is in {}, which patch apply never writes",
            first.to_string_lossy()
        )));
    }
    let mut current = dir.to_path_buf();
    for component in relative.components() {
        current.push(component);
        let is_link = std::fs::symlink_metadata(&current)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        if is_link {
            return Err(usage(format!(
                "{path} goes through a symbolic link, which patch apply never follows"
            )));
        }
    }
    Ok(dir.join(relative))
}

/// The file's text, or `None` when there's no such file.
fn read_existing(full: &Path, path: &str) -> Result<Option<String>, AgentError> {
    match std::fs::read(full) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| usage(format!("{path} isn't UTF-8 text"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(usage(format!("couldn't read {path}: {error}"))),
    }
}

/// One file's new content, or `None` to delete it.
type Planned = (PathBuf, Option<String>);

/// `relish agent patch apply`: match every hunk of `diff` against the
/// workspace, then, unless `check`, write the result.
pub(super) fn apply(dir: &Path, diff: &str, check: bool) -> Result<Answer<ApplyData>, AgentError> {
    State::load(dir)?;
    let patches = parse(diff)?;
    let mut planned: Vec<Planned> = Vec::new();
    let mut files = Vec::new();
    for patch in &patches {
        let (path, status) = match (&patch.old, &patch.new) {
            (None, None) => return Err(usage("a file goes from /dev/null to /dev/null")),
            (Some(old), Some(new)) if old != new => {
                return Err(usage(format!(
                    "renames aren't supported ({old} to {new}); delete one file and add the other"
                )));
            }
            (Some(path), Some(_)) => (path, FileStatus::Modified),
            (None, Some(path)) => (path, FileStatus::Added),
            (Some(path), None) => (path, FileStatus::Deleted),
        };
        if patch.hunks.is_empty() {
            return Err(usage(format!(
                "the diff names {path} but has no hunk for it"
            )));
        }
        let full = workspace_path(dir, path)?;
        // An earlier part of the same diff may have changed this file.
        let current = match planned.iter().rev().find(|(planned, _)| *planned == full) {
            Some((_, content)) => content.clone(),
            None => read_existing(&full, path)?,
        };
        let mut text = match (status, current) {
            (FileStatus::Added, Some(_)) => {
                return Err(usage(format!(
                    "the diff adds {path}, but the workspace already has it"
                )));
            }
            (FileStatus::Added, None) => Text::parse(""),
            (_, None) => {
                return Err(usage(format!("{path} isn't in the workspace")));
            }
            (_, Some(content)) => Text::parse(&content),
        };
        let hunks = apply_hunks(path, &mut text, &patch.hunks)?;
        let content = match status {
            FileStatus::Deleted if !text.lines.is_empty() => {
                return Err(usage(format!(
                    "the diff deletes {path} but leaves {} of its lines",
                    text.lines.len()
                )));
            }
            FileStatus::Deleted => None,
            _ => Some(text.render()),
        };
        planned.push((full, content));
        files.push(AppliedFile {
            path: path.clone(),
            status,
            hunks,
        });
    }
    if !check {
        write(&planned)?;
    }

    let mut answer = answer_of(ApplyData {
        dir: shown(dir),
        checked_only: check,
        files,
    });
    let hunks = || answer.data.files.iter().flat_map(|file| &file.hunks);
    let recounted = hunks().filter(|hunk| hunk.recounted).count();
    let moved = hunks().filter(|hunk| hunk.offset != 0).count();
    if recounted > 0 {
        answer.warnings.push(format!(
            "{recounted} hunk header(s) had wrong line counts; patch diff prints the change with the right ones"
        ));
    }
    if moved > 0 {
        answer.warnings.push(format!(
            "{moved} hunk(s) applied away from the line their header gave"
        ));
    }
    if !check {
        answer.next = vec![
            step(
                patch_command("diff", dir),
                "the change as a unified diff git wrote, with correct hunk headers",
                true,
            ),
            step(
                patch_command("test", dir),
                "the portable suite against the change",
                false,
            ),
        ];
    }
    Ok(answer)
}

/// Write every planned file, or delete it.
fn write(planned: &[Planned]) -> Result<(), AgentError> {
    for (full, content) in planned {
        let result = match content {
            Some(content) => full
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(full, content)),
            None => std::fs::remove_file(full),
        };
        result.map_err(|error| {
            AgentError::new(
                ErrorCode::Internal,
                format!("failed to write {}: {error}", full.display()),
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::fixture::*;
    use super::super::git::git;
    use super::super::workspace;
    use super::*;
    use crate::relish::agent::DEFAULT_MAX_BYTES;
    use crate::relish::agent::source::fixture as source_fixture;
    use crate::relish::agent::source::render_answer;

    /// A fix to `classify_error` in the fixture's `src/client.rs`, written
    /// by hand the way an agent writes one: both hunks' line counts are
    /// wrong, and the second hunk's start line is off by two.
    const MISCOUNTED: &str = "\
--- a/src/client.rs
+++ b/src/client.rs
@@ -12,4 +12,4 @@ impl Client {
     pub fn new() -> Self {
         Client {
-            timeout: Duration::from_secs(10),
+            timeout: Duration::from_secs(30),
         }
     }
@@ -28,6 +28,8 @@ fn classify_error(error: Error) -> Kind {
     if error.is_timeout() {
         Kind::Timeout
+    } else if error.is_connect() {
+        Kind::Refused
     } else {
         Kind::Unreachable
     }
";

    fn read(dir: &Path, path: &str) -> String {
        std::fs::read_to_string(dir.join(path)).unwrap()
    }

    fn status(dir: &Path) -> String {
        git(dir, &["status", "--porcelain"], None).unwrap()
    }

    #[test]
    fn a_diff_with_wrong_hunk_counts_applies_and_diff_prints_a_correct_one() {
        let fixture = Fixture::new("exit 0\n");
        let dir = fixture.dir();
        let answer = apply(&dir, MISCOUNTED, false).unwrap();

        let file = &answer.data.files[0];
        assert_eq!(file.path, "src/client.rs");
        assert_eq!(file.status, FileStatus::Modified);
        let lines: Vec<(u64, i64, bool)> = file
            .hunks
            .iter()
            .map(|hunk| (hunk.line, hunk.offset, hunk.recounted))
            .collect();
        assert_eq!(lines, [(10, -2, true), (27, -1, true)]);
        assert!(read(&dir, "src/client.rs").contains("from_secs(30)"));
        assert!(read(&dir, "src/client.rs").contains("Kind::Refused"));
        assert!(
            answer.warnings.iter().any(|w| w.contains("line counts")),
            "{:?}",
            answer.warnings
        );

        // The canonical diff applies to the extracted source with plain
        // `git apply`, no --recount.
        let printed = workspace::diff(&dir).unwrap().data.diff;
        assert!(printed.contains("@@ -9,7 +9,7 @@"), "{printed}");
        assert!(printed.contains("@@ -26,6 +26,8 @@"), "{printed}");
        git(&dir, &["checkout", "--quiet", "--", "."], None).unwrap();
        let patch = fixture.root.path().join("canonical.diff");
        std::fs::write(&patch, &printed).unwrap();
        git(&dir, &["apply", "--check", patch.to_str().unwrap()], None).unwrap();
    }

    #[test]
    fn a_hunk_whose_context_does_not_match_is_refused_naming_it() {
        let fixture = Fixture::new("exit 0\n");
        let dir = fixture.dir();
        let wrong = MISCOUNTED.replace("        Kind::Timeout\n", "        Kind::TimedOut\n");
        let error = apply(&dir, &wrong, false).unwrap_err();
        assert_eq!(error.code, ErrorCode::Usage);
        assert!(
            error
                .message
                .contains("hunk 2 of src/client.rs (@@ -28,6 +28,8 @@"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("Kind::TimedOut"),
            "{}",
            error.message
        );
        assert!(error.hint.is_some());
        // All or nothing: the first hunk, which matched, isn't written either.
        assert_eq!(status(&dir), "");
    }

    #[test]
    fn check_matches_every_hunk_and_writes_nothing() {
        let fixture = Fixture::new("exit 0\n");
        let dir = fixture.dir();
        let answer = apply(&dir, MISCOUNTED, true).unwrap();
        assert!(answer.data.checked_only);
        assert_eq!(answer.data.files[0].hunks.len(), 2);
        assert_eq!(status(&dir), "");
    }

    #[test]
    fn a_diff_can_add_and_delete_files() {
        let fixture = Fixture::new("exit 0\n");
        let dir = fixture.dir();
        let diff = "\
diff --git a/src/retry.rs b/src/retry.rs
new file mode 100644
--- /dev/null
+++ b/src/retry.rs
@@ -0,0 +1 @@
+pub fn retry() {}
+
diff --git a/src/bin/tool.rs b/src/bin/tool.rs
deleted file mode 100644
--- a/src/bin/tool.rs
+++ /dev/null
@@ -1,3 +0,0 @@
-fn main() {
-    println!(\"hi\");
-}
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1 +1,2 @@
 pub mod client;
+pub mod retry;
";
        let answer = apply(&dir, diff, false).unwrap();
        let statuses: Vec<(&str, FileStatus)> = answer
            .data
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.status))
            .collect();
        assert_eq!(
            statuses,
            [
                ("src/retry.rs", FileStatus::Added),
                ("src/bin/tool.rs", FileStatus::Deleted),
                ("src/lib.rs", FileStatus::Modified),
            ]
        );
        assert_eq!(read(&dir, "src/retry.rs"), "pub fn retry() {}\n\n");
        assert!(!dir.join("src/bin/tool.rs").exists());
        assert_eq!(
            read(&dir, "src/lib.rs"),
            "pub mod client;\npub mod retry;\n"
        );
    }

    #[test]
    fn a_missing_newline_at_the_end_is_kept_as_the_diff_says() {
        let fixture = Fixture::new("exit 0\n");
        let dir = fixture.dir();
        let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1 +1 @@
-pub mod client;
+pub mod client; // no newline
\\ No newline at end of file
";
        apply(&dir, diff, false).unwrap();
        assert_eq!(read(&dir, "src/lib.rs"), "pub mod client; // no newline");
    }

    #[test]
    fn a_blank_context_line_without_its_leading_space_still_matches() {
        let fixture = Fixture::new("exit 0\n");
        let dir = fixture.dir();
        let diff = "\
--- a/docs/book/01-hello.md
+++ b/docs/book/01-hello.md
@@ -1,3 +1,3 @@
 # Hello

-The client times out.
+The client times out after ten seconds.
";
        apply(&dir, diff, false).unwrap();
        assert_eq!(
            read(&dir, "docs/book/01-hello.md"),
            "# Hello\n\nThe client times out after ten seconds.\n"
        );
    }

    #[test]
    fn the_command_stays_inside_the_workspace() {
        let fixture = Fixture::new("exit 0\n");
        let dir = fixture.dir();
        let outside = fixture.root.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, dir.join("src/link")).unwrap();
        let escaping =
            |path: &str| format!("--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n+escaped\n");
        let absolute = outside.join("absolute.txt");
        let mut paths = vec![
            "../escaped.txt".to_string(),
            "src/../../escaped.txt".to_string(),
            ".git/escaped.txt".to_string(),
            ".reliaburger-patch/state.json".to_string(),
        ];
        if cfg!(unix) {
            paths.push("src/link/escaped.txt".to_string());
        }
        for path in &paths {
            let error = apply(&dir, &escaping(path), false).unwrap_err();
            assert_eq!(error.code, ErrorCode::Usage, "{path}: {}", error.message);
        }
        let error = apply(
            &dir,
            &format!(
                "--- /dev/null\n+++ {}\n@@ -0,0 +1 @@\n+escaped\n",
                absolute.display()
            ),
            false,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Usage, "{}", error.message);

        assert!(!fixture.root.path().join("escaped.txt").exists());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        assert!(!dir.join(".git/escaped.txt").exists());
        workspace::State::load(&dir).unwrap();

        // A diff that applies writes only the file it names.
        #[cfg(unix)]
        std::fs::remove_file(dir.join("src/link")).unwrap();
        let before: Vec<_> = std::fs::read_dir(fixture.root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        apply(&dir, MISCOUNTED, false).unwrap();
        let after: Vec<_> = std::fs::read_dir(fixture.root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(before, after);
        assert_eq!(status(&dir).trim(), "M src/client.rs");
    }

    #[test]
    fn a_diff_that_is_not_one_is_a_usage_error() {
        let fixture = Fixture::new("exit 0\n");
        let dir = fixture.dir();
        for text in [
            "",
            "just some words\n",
            "@@ -1 +1 @@\n-a\n+b\n",
            "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ one @@\n-a\n",
            "--- a/src/lib.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-pub mod client;\n+x\n",
            "--- a/src/nothing.rs\n+++ b/src/nothing.rs\n@@ -1 +1 @@\n-a\n+b\n",
        ] {
            let error = apply(&dir, text, false).unwrap_err();
            assert_eq!(error.code, ErrorCode::Usage, "{text:?}: {}", error.message);
        }
        assert_eq!(status(&dir), "");
    }

    #[test]
    fn snapshot_of_an_apply() {
        let fixture = Fixture::new("exit 0\n");
        let result = apply(&fixture.dir(), MISCOUNTED, false);
        let rendered = render_answer(
            crate::relish::agent::CommandName::PatchApply,
            source_fixture::bundle().build_info(),
            result,
            &[],
            DEFAULT_MAX_BYTES,
            "relish agent patch",
        );
        insta::assert_json_snapshot!(normalised(&rendered, fixture.root.path()));
    }
}
