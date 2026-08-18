//! Line-level diffs: what a write or an edit will actually change.
//!
//! This exists so a file change can be *seen* rather than described. Before
//! this, approving a `write_file` meant reading the whole new file and holding
//! the old one in your head, and approving an `edit_file` meant reading a
//! "replace this / with this" pair with no indication of where in the file it
//! landed. Both are the same question -- "what is different afterwards?" -- and
//! both are now answered by the same thing: a `-`/`+` diff with real line
//! numbers, computed against what is on disk right now.
//!
//! Two deliberate limits, because this runs on whatever the model just handed
//! us and a diff is only worth having if it cannot itself become the problem:
//!
//! * The LCS below is O(n·m) in time and memory, so a big-file-versus-big-file
//!   comparison is refused rather than attempted -- see `MAX_CELLS`. The
//!   fallback is still an honest diff, just a blunt one.
//! * Diffs ride along on `Message`, which is serialized to the session file on
//!   disk, so anything kept for the transcript goes through [`FileDiff::clipped`]
//!   first. An unclipped diff of a generated 20k-line file would make the
//!   session log larger than the file it describes.
//!
//! Line endings: both sides are compared by `lines()`, so a change that only
//! adds or removes a trailing newline shows as no change. That is the right
//! trade for something whose whole job is to be read -- a diff whose only
//! entry is an invisible character is noise -- and the byte count in the tool
//! result still reports it.

/// What happened to one line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Change {
    /// Unchanged, shown only to place the changes around it.
    Context,
    Added,
    Removed,
}

/// One line of a rendered diff, already carrying the numbers it displays.
///
/// The numbers are worked out here rather than at render time because the
/// renderer sees hunks in isolation and would have to re-derive an offset it
/// has no way to know. A removed line has no number on the new side and an
/// added line has none on the old side, which is exactly what the gutter shows.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiffLine {
    pub change: Change,
    /// 1-based line number in the old file; `None` for an added line.
    pub old_no: Option<usize>,
    /// 1-based line number in the new file; `None` for a removed line.
    pub new_no: Option<usize>,
    pub text: String,
}

/// A run of changed lines plus the unchanged lines framing it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Hunk {
    pub lines: Vec<DiffLine>,
}

/// Everything one file change does, ready to draw.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileDiff {
    pub hunks: Vec<Hunk>,
    pub added: usize,
    pub removed: usize,
    /// How many lines [`FileDiff::clipped`] dropped off the end. Rendered as
    /// "… N more lines" so a shortened diff never passes for a whole one.
    #[serde(default)]
    pub clipped: usize,
}

impl FileDiff {
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    /// Total lines across every hunk, including context.
    pub fn line_count(&self) -> usize {
        self.hunks.iter().map(|h| h.lines.len()).sum()
    }

    /// "3 additions and 1 removal", in the form a person would say it.
    pub fn tally(&self) -> String {
        let plural = |n: usize, word: &str| {
            format!("{n} {word}{}", if n == 1 { "" } else { "s" })
        };
        match (self.added, self.removed) {
            (0, 0) => "no changes".to_string(),
            (a, 0) => plural(a, "addition"),
            (0, r) => plural(r, "removal"),
            (a, r) => format!("{} and {}", plural(a, "addition"), plural(r, "removal")),
        }
    }

    /// The same diff, cut to at most `max_lines` of displayed diff.
    ///
    /// Cut whole-hunk-first and then mid-hunk, so what survives is always the
    /// beginning of the change rather than a scatter of fragments. `added` and
    /// `removed` are left alone on purpose: the headline count must describe
    /// the *change*, not the excerpt of it that fitted.
    pub fn clipped(mut self, max_lines: usize) -> Self {
        if self.line_count() <= max_lines {
            return self;
        }
        let mut budget = max_lines;
        let mut kept: Vec<Hunk> = Vec::new();
        let mut dropped = 0usize;
        for hunk in self.hunks.drain(..) {
            if budget == 0 {
                dropped += hunk.lines.len();
                continue;
            }
            if hunk.lines.len() <= budget {
                budget -= hunk.lines.len();
                kept.push(hunk);
            } else {
                dropped += hunk.lines.len() - budget;
                let mut lines = hunk.lines;
                lines.truncate(budget);
                budget = 0;
                kept.push(Hunk { lines });
            }
        }
        self.hunks = kept;
        self.clipped = dropped;
        self
    }
}

/// Unchanged lines kept on each side of a change, so a hunk can be read.
const CONTEXT: usize = 3;

/// The most LCS table cells we will allocate: 1M `u32`s, so 4MB at the worst.
///
/// Past this the comparison degrades to "this block became that block" rather
/// than hanging the UI on a quadratic walk. Two 1000-line files fit; a pair of
/// generated 5000-line files does not, and would not have been readable as a
/// line diff anyway.
const MAX_CELLS: usize = 1_000_000;

/// Diff two whole file contents.
pub fn diff(old: &str, new: &str) -> FileDiff {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    build(&a, &b)
}

fn build(a: &[&str], b: &[&str]) -> FileDiff {
    // Trimming the matching head and tail first is what makes this affordable
    // in practice: an edit to one function in a 3000-line file leaves a
    // handful of lines for the quadratic part to look at.
    let head = a
        .iter()
        .zip(b.iter())
        .take_while(|(x, y)| x == y)
        .count();
    let tail = a[head..]
        .iter()
        .rev()
        .zip(b[head..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();

    let mid_a = &a[head..a.len() - tail];
    let mid_b = &b[head..b.len() - tail];

    let mut all: Vec<DiffLine> = Vec::new();
    let mut old_no = 0usize;
    let mut new_no = 0usize;
    let push = |all: &mut Vec<DiffLine>,
                    old_no: &mut usize,
                    new_no: &mut usize,
                    change: Change,
                    text: &str| {
        let (o, n) = match change {
            Change::Context => {
                *old_no += 1;
                *new_no += 1;
                (Some(*old_no), Some(*new_no))
            }
            Change::Added => {
                *new_no += 1;
                (None, Some(*new_no))
            }
            Change::Removed => {
                *old_no += 1;
                (Some(*old_no), None)
            }
        };
        all.push(DiffLine {
            change,
            old_no: o,
            new_no: n,
            text: text.to_string(),
        });
    };

    for line in &a[..head] {
        push(&mut all, &mut old_no, &mut new_no, Change::Context, line);
    }

    if mid_a.len().saturating_mul(mid_b.len()) > MAX_CELLS {
        // Too big to align line by line. Still true, just coarse: everything
        // that was there is gone, everything that is there now is new.
        for line in mid_a {
            push(&mut all, &mut old_no, &mut new_no, Change::Removed, line);
        }
        for line in mid_b {
            push(&mut all, &mut old_no, &mut new_no, Change::Added, line);
        }
    } else {
        for (change, text) in align(mid_a, mid_b) {
            push(&mut all, &mut old_no, &mut new_no, change, text);
        }
    }

    for line in &a[a.len() - tail..] {
        push(&mut all, &mut old_no, &mut new_no, Change::Context, line);
    }

    let added = all.iter().filter(|l| l.change == Change::Added).count();
    let removed = all.iter().filter(|l| l.change == Change::Removed).count();
    FileDiff {
        hunks: into_hunks(all),
        added,
        removed,
        clipped: 0,
    }
}

/// Longest common subsequence, walked back into an ordered edit script.
///
/// Deletions are emitted before insertions at the same position, which is what
/// makes a replacement read as `-old` then `+new` rather than the reverse.
fn align<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<(Change, &'a str)> {
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return b.iter().map(|l| (Change::Added, *l)).collect();
    }
    if m == 0 {
        return a.iter().map(|l| (Change::Removed, *l)).collect();
    }

    // dp[i][j] = length of the LCS of a[i..] and b[j..]. Filled backwards so
    // the walk below can go forwards, which is the order the output needs.
    let stride = m + 1;
    let mut dp = vec![0u32; (n + 1) * stride];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * stride + j] = if a[i] == b[j] {
                dp[(i + 1) * stride + j + 1] + 1
            } else {
                dp[(i + 1) * stride + j].max(dp[i * stride + j + 1])
            };
        }
    }

    let mut out = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((Change::Context, a[i]));
            i += 1;
            j += 1;
        } else if dp[(i + 1) * stride + j] >= dp[i * stride + j + 1] {
            out.push((Change::Removed, a[i]));
            i += 1;
        } else {
            out.push((Change::Added, b[j]));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|l| (Change::Removed, *l)));
    out.extend(b[j..].iter().map(|l| (Change::Added, *l)));
    out
}

/// Keep the changed lines and `CONTEXT` unchanged ones either side, dropping
/// the rest of the file. Runs that would overlap become one hunk rather than
/// two abutting ones, so a pair of edits three lines apart reads as one change.
fn into_hunks(all: Vec<DiffLine>) -> Vec<Hunk> {
    let changed: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, l)| l.change != Change::Context)
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return Vec::new();
    }

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &i in &changed {
        let start = i.saturating_sub(CONTEXT);
        let end = (i + CONTEXT + 1).min(all.len());
        match ranges.last_mut() {
            // `>=`, not `>`: two ranges that merely touch still belong
            // together -- there is nothing between them to elide.
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => ranges.push((start, end)),
        }
    }

    ranges
        .into_iter()
        .map(|(start, end)| Hunk {
            lines: all[start..end].to_vec(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(d: &FileDiff) -> Vec<String> {
        d.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .map(|l| {
                let mark = match l.change {
                    Change::Context => ' ',
                    Change::Added => '+',
                    Change::Removed => '-',
                };
                format!("{mark}{}", l.text)
            })
            .collect()
    }

    #[test]
    fn an_unchanged_file_has_no_hunks() {
        let d = diff("a\nb\nc\n", "a\nb\nc\n");
        assert!(d.is_empty());
        assert_eq!((d.added, d.removed), (0, 0));
        assert_eq!(d.tally(), "no changes");
    }

    #[test]
    fn a_new_file_is_all_additions() {
        let d = diff("", "one\ntwo\n");
        assert_eq!(rendered(&d), vec!["+one", "+two"]);
        assert_eq!((d.added, d.removed), (2, 0));
    }

    #[test]
    fn a_replaced_line_shows_the_old_one_first() {
        let d = diff("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(rendered(&d), vec![" a", "-b", "+B", " c"]);
        assert_eq!(d.tally(), "1 addition and 1 removal");
    }

    #[test]
    fn line_numbers_track_each_side_separately() {
        let d = diff("a\nb\nc\n", "a\nx\ny\nb\nc\n");
        let lines: Vec<_> = d.hunks[0]
            .lines
            .iter()
            .map(|l| (l.change, l.old_no, l.new_no))
            .collect();
        assert_eq!(
            lines,
            vec![
                (Change::Context, Some(1), Some(1)),
                (Change::Added, None, Some(2)),
                (Change::Added, None, Some(3)),
                (Change::Context, Some(2), Some(4)),
                (Change::Context, Some(3), Some(5)),
            ]
        );
    }

    // The whole point of hunks: an edit in a long file must not print the
    // long file.
    #[test]
    fn untouched_stretches_are_elided() {
        let old: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 20\n", "line twenty\n");
        let d = diff(&old, &new);
        assert_eq!(d.hunks.len(), 1);
        assert_eq!(
            rendered(&d),
            vec![
                " line 17", " line 18", " line 19", "-line 20", "+line twenty", " line 21",
                " line 22", " line 23",
            ]
        );
    }

    #[test]
    fn two_distant_changes_become_two_hunks() {
        let old: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 5\n", "five\n").replace("line 35\n", "thirty-five\n");
        let d = diff(&old, &new);
        assert_eq!(d.hunks.len(), 2);
        assert_eq!((d.added, d.removed), (2, 2));
    }

    // Two changes close enough that eliding between them would save nothing.
    #[test]
    fn two_nearby_changes_become_one_hunk() {
        let old: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 20\n", "twenty\n").replace("line 23\n", "twenty-three\n");
        let d = diff(&old, &new);
        assert_eq!(d.hunks.len(), 1);
    }

    #[test]
    fn a_deleted_line_has_no_new_number() {
        let d = diff("a\nb\nc\n", "a\nc\n");
        let removed = d.hunks[0]
            .lines
            .iter()
            .find(|l| l.change == Change::Removed)
            .expect("one removal");
        assert_eq!(removed.text, "b");
        assert_eq!(removed.old_no, Some(2));
        assert_eq!(removed.new_no, None);
        assert_eq!(d.tally(), "1 removal");
    }

    #[test]
    fn clipping_keeps_the_start_and_counts_the_rest() {
        let old: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let new: String = (1..=40).map(|i| format!("changed {i}\n")).collect();
        let full = diff(&old, &new);
        let total = full.line_count();
        let short = full.clone().clipped(10);
        assert_eq!(short.line_count(), 10);
        assert_eq!(short.clipped, total - 10);
        // The headline still describes the change, not the excerpt.
        assert_eq!((short.added, short.removed), (full.added, full.removed));
    }

    #[test]
    fn clipping_a_diff_that_already_fits_changes_nothing() {
        let d = diff("a\nb\n", "a\nB\n");
        let before = d.clone();
        let after = d.clipped(500);
        assert_eq!(after, before);
        assert_eq!(after.clipped, 0);
    }

    // The guard exists so a pathological pair cannot hang the UI. It must
    // still produce something true.
    #[test]
    fn oversized_comparisons_fall_back_without_hanging() {
        let old: String = (0..2000).map(|i| format!("old {i}\n")).collect();
        let new: String = (0..2000).map(|i| format!("new {i}\n")).collect();
        let d = diff(&old, &new);
        assert_eq!(d.removed, 2000);
        assert_eq!(d.added, 2000);
        // Coarse, but in the right order: the removals precede the additions.
        let first = &d.hunks[0].lines[0];
        assert_eq!(first.change, Change::Removed);
    }

    // A file with a matching head and tail must not be dragged into the
    // quadratic path by its size alone.
    #[test]
    fn a_small_edit_to_a_huge_file_is_still_aligned_precisely() {
        let old: String = (0..5000).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 2500\n", "line twenty-five hundred\n");
        let d = diff(&old, &new);
        assert_eq!((d.added, d.removed), (1, 1));
        assert_eq!(d.hunks.len(), 1);
    }

    #[test]
    fn tally_is_singular_for_one() {
        assert_eq!(diff("a\n", "a\nb\n").tally(), "1 addition");
        assert_eq!(diff("a\nb\n", "a\n").tally(), "1 removal");
        assert_eq!(diff("a\n", "b\nc\n").tally(), "2 additions and 1 removal");
    }
}
