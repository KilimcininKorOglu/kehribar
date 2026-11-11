//! Read-only queries on an archived repository for the browsing pages.
//!
//! Every revision and path comes from a URL, so both are validated before
//! they reach `git`: a revision cannot start with `-` or carry revision
//! syntax, and a path cannot leave the repository tree.

use std::collections::HashMap;
use std::path::Path;

use super::command::{Git, GitError};
use super::mirror::{ARCHIVE_REF_PREFIX, REWRITTEN_REF_PREFIX};

/// Largest file shown inline; a larger one is offered as a download.
pub const MAX_BLOB_BYTES: usize = 1024 * 1024;
/// Largest diff read for one commit; the rest is cut.
pub const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;
const MAX_REV_LEN: usize = 255;
const MAX_PATH_LEN: usize = 4096;

/// A revision that is safe to pass to `git`: a ref name or an object id.
pub fn valid_rev(rev: &str) -> bool {
    !rev.is_empty()
        && rev.len() <= MAX_REV_LEN
        && !rev.starts_with('-')
        && !rev.contains("..")
        && !rev.contains("@{")
        && rev.chars().all(|c| {
            !c.is_control() && !matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        })
}

/// A path inside the repository tree, without leading or trailing slash.
/// Returns the normalised path, or `None` when it is not acceptable.
pub fn clean_path(path: &str) -> Option<String> {
    let trimmed = path.trim_matches('/');
    if trimmed.len() > MAX_PATH_LEN || trimmed.contains(['\0', '\n']) {
        return None;
    }
    let ok = trimmed.is_empty()
        || trimmed
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..");
    ok.then(|| trimmed.to_owned())
}

/// The full object id of the commit `rev` names, or `None` when it names
/// no commit. `rev` must have passed [`valid_rev`].
pub async fn resolve_commit(git: &Git, dir: &Path, rev: &str) -> Result<Option<String>, GitError> {
    let spec = format!("{rev}^{{commit}}");
    let args = [
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        &spec,
    ];
    // --quiet exits with 1 and no message when the name is unknown.
    run_or_missing(git, dir, &args, 1).await
}

text_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RefKind {
        Branch => "branch",
        Tag => "tag",
        /// A commit kept after the source rewrote the ref.
        Rescued => "rescued",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    pub kind: RefKind,
    /// Full ref name, which the browsing pages accept as a revision.
    pub name: String,
    /// The name without its `refs/...` prefix.
    pub short: String,
    /// The commit the ref points to (annotated tags are peeled).
    pub commit: String,
    pub time: i64,
    pub subject: String,
}

/// Branches, tags and rescued refs, newest first within each kind.
pub async fn refs(git: &Git, dir: &Path) -> Result<Vec<RefEntry>, GitError> {
    let format = "--format=%(refname)%00%(objectname)%00%(*objectname)%00%(creatordate:unix)%00%(contents:subject)";
    let args = [
        "for-each-ref",
        "--sort=-creatordate",
        format,
        "refs/heads/",
        "refs/tags/",
        ARCHIVE_REF_PREFIX,
    ];
    let out = git.run(Some(dir), args, None).await?;
    let text = String::from_utf8_lossy(&out);
    let mut list: Vec<RefEntry> = text.lines().filter_map(parse_ref).collect();
    list.sort_by_key(|r| r.kind as u8);
    Ok(list)
}

fn parse_ref(line: &str) -> Option<RefEntry> {
    let mut fields = line.split('\0');
    let name = fields.next()?;
    let object = fields.next()?;
    let peeled = fields.next()?;
    let time = fields.next()?.parse().unwrap_or(0);
    let subject = fields.next().unwrap_or_default();
    let (kind, short) = if let Some(short) = name.strip_prefix("refs/heads/") {
        (RefKind::Branch, short)
    } else if let Some(short) = name.strip_prefix("refs/tags/") {
        (RefKind::Tag, short)
    } else {
        (RefKind::Rescued, name.strip_prefix(REWRITTEN_REF_PREFIX)?)
    };
    Some(RefEntry {
        kind,
        name: name.to_owned(),
        short: short.to_owned(),
        commit: if peeled.is_empty() { object } else { peeled }.to_owned(),
        time,
        subject: subject.to_owned(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSummary {
    pub sha: String,
    pub author: String,
    pub time: i64,
    pub subject: String,
}

/// One page of the history of `commit`. `search` keeps the commits whose
/// message contains the text, ignoring case.
pub async fn log(
    git: &Git,
    dir: &Path,
    commit: &str,
    search: Option<&str>,
    skip: usize,
    limit: usize,
) -> Result<Vec<CommitSummary>, GitError> {
    let mut args = vec![
        "log".to_owned(),
        "--no-color".to_owned(),
        "--format=%H%x00%an%x00%at%x00%s%x1e".to_owned(),
        format!("--skip={skip}"),
        format!("--max-count={limit}"),
    ];
    if let Some(text) = search {
        args.push("--regexp-ignore-case".to_owned());
        args.push("--fixed-strings".to_owned());
        args.push(format!("--grep={text}"));
    }
    args.extend([
        "--end-of-options".to_owned(),
        commit.to_owned(),
        "--".to_owned(),
    ]);
    let out = git.run(Some(dir), &args, None).await?;
    let text = String::from_utf8_lossy(&out);
    Ok(text
        .split('\x1e')
        .filter_map(|record| parse_summary(record.trim_start_matches('\n')))
        .collect())
}

fn parse_summary(record: &str) -> Option<CommitSummary> {
    let mut fields = record.split('\0');
    Some(CommitSummary {
        sha: fields.next().filter(|s| !s.is_empty())?.to_owned(),
        author: fields.next()?.to_owned(),
        time: fields.next()?.parse().ok()?,
        subject: fields.next().unwrap_or_default().to_owned(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDetail {
    pub sha: String,
    pub parents: Vec<String>,
    pub author: String,
    pub author_email: String,
    pub author_time: i64,
    pub committer: String,
    pub committer_time: i64,
    pub message: String,
}

pub async fn commit(git: &Git, dir: &Path, sha: &str) -> Result<Option<CommitDetail>, GitError> {
    let format = "--format=%H%x00%P%x00%an%x00%ae%x00%at%x00%cn%x00%ct%x00%B";
    let args = ["show", "-s", "--no-color", format, "--end-of-options", sha];
    let out = git.run(Some(dir), args, None).await?;
    Ok(parse_commit(&String::from_utf8_lossy(&out)))
}

fn parse_commit(text: &str) -> Option<CommitDetail> {
    let fields: Vec<&str> = text.splitn(8, '\0').collect();
    let [
        sha,
        parents,
        author,
        email,
        author_time,
        committer,
        committer_time,
        message,
    ] = fields.as_slice()
    else {
        return None;
    };
    Some(CommitDetail {
        sha: (*sha).to_owned(),
        parents: parents.split_whitespace().map(str::to_owned).collect(),
        author: (*author).to_owned(),
        author_email: (*email).to_owned(),
        author_time: author_time.parse().ok()?,
        committer: (*committer).to_owned(),
        committer_time: committer_time.parse().ok()?,
        message: message.trim_end().to_owned(),
    })
}

text_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FileChange {
        Added => "added",
        Deleted => "deleted",
        Renamed => "renamed",
        Modified => "modified",
    }
}

text_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum LineKind {
        Hunk => "hunk",
        Added => "add",
        Removed => "del",
        Context => "ctx",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub old_path: String,
    pub new_path: String,
    pub change: FileChange,
    pub binary: bool,
    pub added: usize,
    pub removed: usize,
    pub lines: Vec<DiffLine>,
}

impl FileDiff {
    fn new(header: &str) -> Self {
        // "a/x b/y": the split is exact unless a name contains " b/"; the
        // ---/+++ lines that follow correct the names when there are hunks.
        let names = header.trim_start_matches("diff --git ");
        let (old, new) = names.rsplit_once(" b/").unwrap_or((names, names));
        let old = old.strip_prefix("a/").unwrap_or(old);
        Self {
            old_path: old.to_owned(),
            new_path: new.to_owned(),
            change: FileChange::Modified,
            binary: false,
            added: 0,
            removed: 0,
            lines: Vec::new(),
        }
    }

    /// The name to show: the new one, or the old one of a deleted file.
    pub fn path(&self) -> &str {
        if self.change == FileChange::Deleted {
            &self.old_path
        } else {
            &self.new_path
        }
    }

    fn header_line(&mut self, line: &str) {
        if line.starts_with("new file mode") {
            self.change = FileChange::Added;
        } else if line.starts_with("deleted file mode") {
            self.change = FileChange::Deleted;
        } else if let Some(name) = line.strip_prefix("rename from ") {
            self.change = FileChange::Renamed;
            name.clone_into(&mut self.old_path);
        } else if let Some(name) = line.strip_prefix("rename to ") {
            name.clone_into(&mut self.new_path);
        } else if line.starts_with("Binary files ") {
            self.binary = true;
        } else if let Some(name) = line.strip_prefix("--- a/") {
            name.clone_into(&mut self.old_path);
        } else if let Some(name) = line.strip_prefix("+++ b/") {
            name.clone_into(&mut self.new_path);
        }
    }

    fn body_line(&mut self, line: &str) {
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Added, &line[1..]),
            Some(b'-') => (LineKind::Removed, &line[1..]),
            Some(b'@') => (LineKind::Hunk, line),
            // "\ No newline at end of file" and context lines.
            Some(b' ') => (LineKind::Context, &line[1..]),
            _ => (LineKind::Context, line),
        };
        match kind {
            LineKind::Added => self.added += 1,
            LineKind::Removed => self.removed += 1,
            _ => {}
        }
        self.lines.push(DiffLine {
            kind,
            text: text.to_owned(),
        });
    }
}

/// The changes of `sha` against its first parent, and whether the diff was
/// cut at [`MAX_DIFF_BYTES`].
pub async fn diff(git: &Git, dir: &Path, sha: &str) -> Result<(Vec<FileDiff>, bool), GitError> {
    let args = [
        "-c",
        "core.quotePath=false",
        "show",
        "--format=",
        "--patch",
        "--find-renames",
        "--no-color",
        "--no-ext-diff",
        "--diff-merges=first-parent",
        "--end-of-options",
        sha,
    ];
    let (out, cut) = git.run_capped(dir, &args, MAX_DIFF_BYTES).await?;
    Ok((parse_diff(&String::from_utf8_lossy(&out)), cut))
}

/// The changes of `head` since its merge base with `base` (what `head`
/// adds), or against `base` itself when the two share no history; and
/// whether the diff was cut at [`MAX_DIFF_BYTES`]. Both are commit ids.
pub async fn compare_diff(
    git: &Git,
    dir: &Path,
    base: &str,
    head: &str,
) -> Result<(Vec<FileDiff>, bool), GitError> {
    let from = merge_base(git, dir, base, head)
        .await?
        .unwrap_or_else(|| base.to_owned());
    let args = [
        "-c",
        "core.quotePath=false",
        "diff",
        "--patch",
        "--find-renames",
        "--no-color",
        "--no-ext-diff",
        "--end-of-options",
        &from,
        head,
        "--",
    ];
    let (out, cut) = git.run_capped(dir, &args, MAX_DIFF_BYTES).await?;
    Ok((parse_diff(&String::from_utf8_lossy(&out)), cut))
}

/// The best common ancestor of two commits; `None` when they share none.
async fn merge_base(git: &Git, dir: &Path, a: &str, b: &str) -> Result<Option<String>, GitError> {
    // merge-base exits with 1 and no output when there is no common ancestor.
    run_or_missing(git, dir, &["merge-base", "--end-of-options", a, b], 1).await
}

/// The number of commits in `head` that `base` does not have.
pub async fn count_ahead(git: &Git, dir: &Path, base: &str, head: &str) -> Result<i64, GitError> {
    let range = format!("{base}..{head}");
    let args = ["rev-list", "--count", "--end-of-options", &range, "--"];
    let out = git.run(Some(dir), args, None).await?;
    Ok(String::from_utf8_lossy(&out).trim().parse().unwrap_or(0))
}

/// The output `grep` reads at most; past it the result is cut.
const MAX_GREP_BYTES: usize = 4 * 1024 * 1024;
/// A matching line longer than this is cut, so one line cannot fill a page.
const MAX_GREP_LINE: usize = 400;

/// One matching line of [`grep`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    pub path: String,
    pub line: u64,
    pub text: String,
}

/// How [`grep`] reads its pattern.
#[derive(Debug, Clone, Copy)]
pub struct GrepOptions<'a> {
    pub pattern: &'a str,
    /// An extended regular expression; else a fixed text.
    pub regex: bool,
    pub ignore_case: bool,
}

/// The lines of the text files of `commit` that match, at most `limit`,
/// and whether more were found. Binary files are skipped. An invalid
/// regular expression fails with exit code 128.
pub async fn grep(
    git: &Git,
    dir: &Path,
    commit: &str,
    options: GrepOptions<'_>,
    limit: usize,
) -> Result<(Vec<GrepMatch>, bool), GitError> {
    let mut args = vec!["grep", "-n", "-I", "--null", "--no-color"];
    if options.ignore_case {
        args.push("-i");
    }
    args.push(if options.regex { "-E" } else { "-F" });
    // `-e` keeps a pattern that starts with `-` from being read as an option.
    args.extend(["-e", options.pattern, commit, "--"]);
    let out = match git.run_capped(dir, &args, MAX_GREP_BYTES).await {
        Ok((out, cut)) => (out, cut),
        // grep exits with 1 when nothing matches.
        Err(err) if err.exit_code == Some(1) => return Ok((Vec::new(), false)),
        Err(err) => return Err(err),
    };
    let text = String::from_utf8_lossy(&out.0);
    let prefix = format!("{commit}:");
    let mut matches: Vec<GrepMatch> = text
        .lines()
        .filter_map(|line| parse_grep_line(line, &prefix))
        .take(limit + 1)
        .collect();
    let cut = out.1 || matches.len() > limit;
    matches.truncate(limit);
    Ok((matches, cut))
}

/// Reads `<commit>:<path>\0<line><sep><text>`; the separator after the
/// line number is `\0` or `:`, depending on the Git version.
fn parse_grep_line(line: &str, prefix: &str) -> Option<GrepMatch> {
    let (name, rest) = line.split_once('\0')?;
    let path = name.strip_prefix(prefix).unwrap_or(name);
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let number = rest.get(..digits)?.parse().ok()?;
    let text = rest.get(digits + 1..).unwrap_or_default();
    let text = match text.char_indices().nth(MAX_GREP_LINE) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    };
    Some(GrepMatch {
        path: path.to_owned(),
        line: number,
        text,
    })
}

pub fn parse_diff(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut in_hunks = false;
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            files.push(FileDiff::new(line));
            in_hunks = false;
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        in_hunks |= line.starts_with("@@");
        if in_hunks {
            file.body_line(line);
        } else {
            file.header_line(line);
        }
    }
    files
}

text_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub enum EntryKind {
        Tree => "tree",
        /// A submodule: a commit of another repository.
        Submodule => "commit",
        Blob => "blob",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub kind: EntryKind,
    pub name: String,
    /// Path from the repository root.
    pub path: String,
    /// Size in bytes; files only.
    pub size: Option<u64>,
    pub symlink: bool,
}

/// What a path names in a commit, or `None` when it names nothing.
pub async fn object_kind(
    git: &Git,
    dir: &Path,
    commit: &str,
    path: &str,
) -> Result<Option<EntryKind>, GitError> {
    let spec = object_spec(commit, path);
    let out = run_or_missing(git, dir, &["cat-file", "-t", &spec], 128).await?;
    Ok(out.and_then(|text| EntryKind::parse(&text)))
}

/// The trimmed output of `git <args>`, or `None` when git exits with
/// `missing_code`, its answer for "no such object".
async fn run_or_missing(
    git: &Git,
    dir: &Path,
    args: &[&str],
    missing_code: i32,
) -> Result<Option<String>, GitError> {
    match git.run(Some(dir), args, None).await {
        Ok(out) => Ok(Some(String::from_utf8_lossy(&out).trim().to_owned())),
        Err(err) if err.exit_code == Some(missing_code) => Ok(None),
        Err(err) => Err(err),
    }
}

/// `<commit>:<path>`, or the root tree of the commit when `path` is empty.
fn object_spec(commit: &str, path: &str) -> String {
    if path.is_empty() {
        format!("{commit}^{{tree}}")
    } else {
        format!("{commit}:{path}")
    }
}

/// The entries of the directory `path`, directories first.
pub async fn tree(
    git: &Git,
    dir: &Path,
    commit: &str,
    path: &str,
) -> Result<Vec<TreeEntry>, GitError> {
    let spec = object_spec(commit, path);
    let out = git
        .run(Some(dir), ["ls-tree", "-z", "--long", &spec], None)
        .await?;
    let text = String::from_utf8_lossy(&out);
    let mut entries: Vec<TreeEntry> = text
        .split('\0')
        .filter_map(|record| parse_tree_entry(record, path))
        .collect();
    entries.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    Ok(entries)
}

/// `<mode> <type> <object> <size>\t<name>`.
fn parse_tree_entry(record: &str, parent: &str) -> Option<TreeEntry> {
    let (meta, name) = record.split_once('\t')?;
    let mut fields = meta.split_whitespace();
    let mode = fields.next()?;
    let kind = EntryKind::parse(fields.next()?)?;
    let size = fields.nth(1).and_then(|s| s.parse().ok());
    let path = if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    };
    Some(TreeEntry {
        kind,
        name: name.to_owned(),
        path,
        size,
        symlink: mode == "120000",
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub size: usize,
    /// The content, empty when the file is larger than the limit.
    pub data: Vec<u8>,
    pub too_large: bool,
}

impl Blob {
    /// Git's own test: a NUL byte in the first 8000 bytes means binary.
    pub fn is_binary(&self) -> bool {
        self.data.iter().take(8000).any(|b| *b == 0)
    }
}

/// The file at `path`, read only up to `limit` bytes.
pub async fn blob(
    git: &Git,
    dir: &Path,
    commit: &str,
    path: &str,
    limit: usize,
) -> Result<Blob, GitError> {
    let spec = object_spec(commit, path);
    let size_text = git.run(Some(dir), ["cat-file", "-s", &spec], None).await?;
    let size = String::from_utf8_lossy(&size_text)
        .trim()
        .parse()
        .unwrap_or(usize::MAX);
    if size > limit {
        return Ok(Blob {
            size,
            data: Vec::new(),
            too_large: true,
        });
    }
    let (data, _) = git
        .run_capped(dir, &["cat-file", "blob", &spec], limit)
        .await?;
    Ok(Blob {
        size,
        data,
        too_large: false,
    })
}

/// The size of the file at `path` and its first `count` bytes, which are
/// enough to tell text from binary.
pub async fn peek(
    git: &Git,
    dir: &Path,
    commit: &str,
    path: &str,
    count: usize,
) -> Result<Blob, GitError> {
    let spec = object_spec(commit, path);
    let size_text = git.run(Some(dir), ["cat-file", "-s", &spec], None).await?;
    let (data, cut) = git
        .run_capped(dir, &["cat-file", "blob", &spec], count)
        .await?;
    Ok(Blob {
        size: String::from_utf8_lossy(&size_text)
            .trim()
            .parse()
            .unwrap_or(0),
        data,
        too_large: cut,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameCommit {
    pub author: String,
    pub time: i64,
    pub summary: String,
}

/// Consecutive lines last changed by one commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameChunk {
    pub sha: String,
    pub first_line: usize,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Blame {
    pub chunks: Vec<BlameChunk>,
    pub commits: HashMap<String, BlameCommit>,
}

pub async fn blame(git: &Git, dir: &Path, commit: &str, path: &str) -> Result<Blame, GitError> {
    let args = ["blame", "--porcelain", commit, "--", path];
    let out = git.run(Some(dir), args, None).await?;
    Ok(parse_blame(&String::from_utf8_lossy(&out)))
}

pub fn parse_blame(text: &str) -> Blame {
    let mut blame = Blame::default();
    let mut current: Option<(String, usize)> = None;
    let mut info: HashMap<&str, String> = HashMap::new();
    for line in text.lines() {
        if let Some(content) = line.strip_prefix('\t') {
            if let Some((sha, number)) = current.take() {
                blame.remember(&sha, &mut info);
                blame.push_line(sha, number, content);
            }
        } else if current.is_none() {
            current = blame_header(line);
        } else if let Some((key, value)) = line.split_once(' ')
            && matches!(key, "author" | "author-time" | "summary")
        {
            info.insert(key, value.to_owned());
        }
    }
    blame
}

/// `<sha> <original line> <final line> [<group size>]`.
fn blame_header(line: &str) -> Option<(String, usize)> {
    let mut fields = line.split(' ');
    let sha = fields.next()?;
    let number = fields.nth(1)?.parse().ok()?;
    (sha.len() >= 40).then(|| (sha.to_owned(), number))
}

impl Blame {
    /// Stores the commit details that porcelain output gives only on the
    /// first line of each commit.
    fn remember(&mut self, sha: &str, info: &mut HashMap<&str, String>) {
        if self.commits.contains_key(sha) || info.is_empty() {
            return;
        }
        let commit = BlameCommit {
            author: info.remove("author").unwrap_or_default(),
            time: info
                .remove("author-time")
                .and_then(|t| t.parse().ok())
                .unwrap_or(0),
            summary: info.remove("summary").unwrap_or_default(),
        };
        self.commits.insert(sha.to_owned(), commit);
        info.clear();
    }

    fn push_line(&mut self, sha: String, number: usize, content: &str) {
        match self.chunks.last_mut() {
            Some(chunk) if chunk.sha == sha => chunk.lines.push(content.to_owned()),
            _ => self.chunks.push(BlameChunk {
                sha,
                first_line: number,
                lines: vec![content.to_owned()],
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grep_lines_give_path_line_and_text() {
        let both = [
            "abc:dir/a b.txt\x0012\x00let x = 1;",
            "abc:dir/a b.txt\x0012:let x = 1;",
        ];
        for line in both {
            assert_eq!(
                parse_grep_line(line, "abc:"),
                Some(GrepMatch {
                    path: "dir/a b.txt".into(),
                    line: 12,
                    text: "let x = 1;".into()
                }),
                "{line:?}"
            );
        }
        let long = format!("abc:f\x001\x00{}", "é".repeat(MAX_GREP_LINE + 5));
        let cut = parse_grep_line(&long, "abc:").unwrap();
        assert_eq!(cut.text.chars().count(), MAX_GREP_LINE + 1);
        assert!(cut.text.ends_with('…'));
        assert_eq!(parse_grep_line("no separator", "abc:"), None);
    }

    #[test]
    fn revisions_with_options_or_revision_syntax_are_refused() {
        for ok in [
            "main",
            "feature/x",
            "v1.0",
            "refs/git-archive/rewritten/1/refs/heads/main",
            "0a1b2c",
        ] {
            assert!(valid_rev(ok), "{ok}");
        }
        for bad in [
            "",
            "-n",
            "--output=x",
            "a..b",
            "main~1",
            "main^",
            "HEAD:path",
            "@{1}",
            "a b",
            "x\ny",
        ] {
            assert!(!valid_rev(bad), "{bad:?}");
        }
    }

    #[test]
    fn paths_cannot_leave_the_tree() {
        assert_eq!(clean_path("/src/main.rs/").as_deref(), Some("src/main.rs"));
        assert_eq!(clean_path("").as_deref(), Some(""));
        assert_eq!(clean_path("a b/c:d").as_deref(), Some("a b/c:d"));
        for bad in ["../etc", "a/../b", "a//b", "./a", "a\0b"] {
            assert_eq!(clean_path(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn diff_is_split_into_files_and_lines() {
        let text = "diff --git a/old.txt b/new.txt\n\
            similarity index 90%\n\
            rename from old.txt\n\
            rename to new.txt\n\
            --- a/old.txt\n\
            +++ b/new.txt\n\
            @@ -1,2 +1,2 @@\n\
             same\n\
            -gone\n\
            +came\n\
            diff --git a/img.png b/img.png\n\
            new file mode 100644\n\
            Binary files /dev/null and b/img.png differ\n\
            diff --git a/x b/x\n\
            deleted file mode 100644\n\
            --- a/x\n\
            +++ /dev/null\n\
            @@ -1 +0,0 @@\n\
            --- not a header\n";
        let files = parse_diff(text);
        assert_eq!(files.len(), 3);
        let renamed = &files[0];
        assert_eq!(renamed.change, FileChange::Renamed);
        assert_eq!(
            (renamed.old_path.as_str(), renamed.path()),
            ("old.txt", "new.txt")
        );
        assert_eq!((renamed.added, renamed.removed), (1, 1));
        assert_eq!(renamed.lines[0].kind, LineKind::Hunk);
        assert_eq!(
            renamed.lines[1],
            DiffLine {
                kind: LineKind::Context,
                text: "same".into()
            }
        );
        assert!(files[1].binary);
        assert_eq!(files[1].change, FileChange::Added);
        // A removed line that starts with "--" stays a removed line.
        assert_eq!(files[2].change, FileChange::Deleted);
        assert_eq!(files[2].path(), "x");
        assert_eq!(files[2].removed, 1);
        assert_eq!(files[2].lines[1].text, "-- not a header");
    }

    #[test]
    fn blame_groups_consecutive_lines_of_one_commit() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let text = format!(
            "{a} 1 1 2\nauthor Ann\nauthor-time 100\nsummary first\nfilename f\n\tline one\n\
             {a} 2 2\n\tline two\n\
             {b} 1 3 1\nauthor Bob\nauthor-time 200\nsummary second\nfilename f\n\tline three\n"
        );
        let blame = parse_blame(&text);
        assert_eq!(blame.chunks.len(), 2);
        assert_eq!(blame.chunks[0].lines, ["line one", "line two"]);
        assert_eq!(blame.chunks[1].first_line, 3);
        assert_eq!(blame.commits[&a].author, "Ann");
        assert_eq!(blame.commits[&b].summary, "second");
        assert_eq!(blame.commits[&b].time, 200);
    }

    #[test]
    fn tree_entries_carry_kind_size_and_path() {
        let entry = parse_tree_entry("100644 blob 0123 42\tREADME.md", "docs").unwrap();
        assert_eq!(entry.kind, EntryKind::Blob);
        assert_eq!(entry.size, Some(42));
        assert_eq!(entry.path, "docs/README.md");
        let dir = parse_tree_entry("040000 tree 0123       -\tsrc", "").unwrap();
        assert_eq!(
            (dir.kind, dir.size, dir.path.as_str()),
            (EntryKind::Tree, None, "src")
        );
        let link = parse_tree_entry("120000 blob 0123 7\tlink", "").unwrap();
        assert!(link.symlink);
    }

    #[test]
    fn commit_fields_are_split_and_short_records_refused() {
        let text = [
            "abc",
            "p1 p2",
            "Ann",
            "a@x",
            "10",
            "Cem",
            "20",
            "Subject\n\nBody\n",
        ]
        .join("\0");
        let commit = parse_commit(&text).unwrap();
        assert_eq!(commit.parents, ["p1", "p2"]);
        assert_eq!((commit.author_time, commit.committer_time), (10, 20));
        assert_eq!(commit.message, "Subject\n\nBody");
        assert_eq!(parse_commit("abc\0p1"), None);
    }

    #[test]
    fn refs_are_classified_and_tags_peeled() {
        let record = |fields: &[&str]| fields.join("\0");
        let tag = parse_ref(&record(&["refs/tags/v1", "aaa", "bbb", "100", "Release"])).unwrap();
        assert_eq!(
            (tag.kind, tag.short.as_str(), tag.commit.as_str(), tag.time),
            (RefKind::Tag, "v1", "bbb", 100)
        );
        let rescued = parse_ref(&record(&[
            "refs/git-archive/rewritten/5/heads/main",
            "ccc",
            "",
            "0",
            "",
        ]))
        .unwrap();
        assert_eq!(rescued.kind, RefKind::Rescued);
        assert_eq!(rescued.short, "5/heads/main");
        assert_eq!(rescued.commit, "ccc");
        assert_eq!(
            parse_ref(&record(&["refs/notes/x", "a", "", "0", ""])),
            None
        );
    }
}
