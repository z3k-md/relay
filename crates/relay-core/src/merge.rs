//! Line-oriented three-way merge for text files.
//!
//! A clean merge is deterministic and independent of which side is "local".
//! Overlapping edits return [`MergeOutcome::Conflict`]; the engine then keeps
//! both versions as conflict copies.

/// Largest file we will try to merge. Larger files stay on the conflict-copy path.
const MAX_MERGE_BYTES: usize = 1024 * 1024;
/// Bound on the LCS table (`n * m` line cells).
const MAX_LCS_CELLS: usize = 400_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    Clean(Vec<u8>),
    Conflict,
}

/// True for UTF-8 without NUL bytes, within [`MAX_MERGE_BYTES`]. Empty is text.
fn is_mergeable_text(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_MERGE_BYTES && !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

/// Merge `left` and `right` against `base`. Sides are symmetric.
pub fn merge_text(base: &[u8], left: &[u8], right: &[u8]) -> MergeOutcome {
    if !is_mergeable_text(base) || !is_mergeable_text(left) || !is_mergeable_text(right) {
        return MergeOutcome::Conflict;
    }
    let base = std::str::from_utf8(base).unwrap_or("");
    let left = std::str::from_utf8(left).unwrap_or("");
    let right = std::str::from_utf8(right).unwrap_or("");
    let base_lines = split_lines(base);
    let left_lines = split_lines(left);
    let right_lines = split_lines(right);
    let Some(a) = hunks(&base_lines, &left_lines) else {
        return MergeOutcome::Conflict;
    };
    let Some(b) = hunks(&base_lines, &right_lines) else {
        return MergeOutcome::Conflict;
    };
    match merge_hunks(&base_lines, &a, &b) {
        Some(text) => MergeOutcome::Clean(text.into_bytes()),
        None => MergeOutcome::Conflict,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Hunk {
    start: usize,
    end: usize,
    lines: Vec<String>,
}

#[derive(Clone, Copy)]
enum Op {
    Equal,
    Delete,
    Insert,
}

fn split_lines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i..].iter().position(|c| *c == b'\n') {
            Some(rel) => {
                out.push(&s[i..i + rel + 1]);
                i += rel + 1;
            }
            None => {
                out.push(&s[i..]);
                break;
            }
        }
    }
    out
}

fn hunks(base: &[&str], edited: &[&str]) -> Option<Vec<Hunk>> {
    let ops = diff_ops(base, edited)?;
    let mut hunks = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    let mut cur: Option<Hunk> = None;
    for op in ops {
        match op {
            Op::Equal => {
                if let Some(h) = cur.take() {
                    hunks.push(h);
                }
                i += 1;
                j += 1;
            }
            Op::Delete => {
                let h = cur.get_or_insert(Hunk {
                    start: i,
                    end: i,
                    lines: Vec::new(),
                });
                i += 1;
                h.end = i;
            }
            Op::Insert => {
                let h = cur.get_or_insert(Hunk {
                    start: i,
                    end: i,
                    lines: Vec::new(),
                });
                h.lines.push(edited[j].to_owned());
                j += 1;
            }
        }
    }
    if let Some(h) = cur {
        hunks.push(h);
    }
    Some(hunks)
}

fn diff_ops(base: &[&str], edited: &[&str]) -> Option<Vec<Op>> {
    let n = base.len();
    let m = edited.len();
    if n.saturating_mul(m) > MAX_LCS_CELLS {
        return None;
    }
    let cols = m + 1;
    let mut dp = vec![0u32; (n + 1) * cols];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * cols + j] = if base[i] == edited[j] {
                dp[(i + 1) * cols + (j + 1)] + 1
            } else {
                dp[(i + 1) * cols + j].max(dp[i * cols + (j + 1)])
            };
        }
    }
    let mut ops = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < n || j < m {
        if i < n
            && j < m
            && base[i] == edited[j]
            && dp[i * cols + j] == dp[(i + 1) * cols + (j + 1)] + 1
        {
            ops.push(Op::Equal);
            i += 1;
            j += 1;
        } else if j < m && (i == n || dp[i * cols + j] == dp[i * cols + (j + 1)]) {
            ops.push(Op::Insert);
            j += 1;
        } else if i < n {
            ops.push(Op::Delete);
            i += 1;
        } else {
            return None;
        }
    }
    Some(ops)
}

fn merge_hunks(base: &[&str], a: &[Hunk], b: &[Hunk]) -> Option<String> {
    let mut out = String::new();
    let mut pos = 0usize;
    let mut ia = 0usize;
    let mut ib = 0usize;
    let limit = base.len() + a.len() + b.len() + 2;
    let mut steps = 0usize;
    while pos < base.len() || ia < a.len() || ib < b.len() {
        steps += 1;
        if steps > limit {
            return None;
        }
        let a_here = ia < a.len() && a[ia].start == pos;
        let b_here = ib < b.len() && b[ib].start == pos;
        if a_here && b_here {
            let ha = &a[ia];
            let hb = &b[ib];
            if ha.end == hb.end && ha.lines == hb.lines {
                push_lines(&mut out, &ha.lines);
                pos = ha.end;
                ia += 1;
                ib += 1;
                continue;
            }
            return None;
        } else if a_here {
            let ha = &a[ia];
            if ib < b.len() && b[ib].start < ha.end {
                return None;
            }
            push_lines(&mut out, &ha.lines);
            let next = ha.end;
            ia += 1;
            pos = next;
        } else if b_here {
            let hb = &b[ib];
            if hb.start != hb.end && ia < a.len() && a[ia].start < hb.end {
                return None;
            }
            push_lines(&mut out, &hb.lines);
            let next = hb.end;
            ib += 1;
            pos = next;
        } else {
            let next_a = a.get(ia).map(|h| h.start).unwrap_or(base.len());
            let next_b = b.get(ib).map(|h| h.start).unwrap_or(base.len());
            let next = next_a.min(next_b).min(base.len());
            if next <= pos {
                if ia >= a.len() && ib >= b.len() {
                    break;
                }
                return None;
            }
            while pos < next {
                out.push_str(base[pos]);
                pos += 1;
            }
        }
    }
    Some(out)
}

fn push_lines(out: &mut String, lines: &[String]) {
    for line in lines {
        out.push_str(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonoverlapping_edits_merge_and_are_symmetric() {
        let base = b"alpha\nbeta\n";
        let left = b"ALPHA\nbeta\n";
        let right = b"alpha\nBETA\n";
        let merged = b"ALPHA\nBETA\n";
        assert_eq!(
            merge_text(base, left, right),
            MergeOutcome::Clean(merged.to_vec())
        );
        assert_eq!(
            merge_text(base, right, left),
            MergeOutcome::Clean(merged.to_vec())
        );
    }

    #[test]
    fn overlapping_edits_conflict() {
        let base = b"base\n";
        assert_eq!(
            merge_text(base, b"alpha\n", b"bravo\n"),
            MergeOutcome::Conflict
        );
    }

    #[test]
    fn identical_edits_merge_clean() {
        let base = b"base\n";
        let edited = b"same\n";
        assert_eq!(
            merge_text(base, edited, edited),
            MergeOutcome::Clean(edited.to_vec())
        );
    }

    #[test]
    fn inserts_at_different_places_merge() {
        let base = b"a\nb\n";
        let left = b"a\nx\nb\n";
        let right = b"a\nb\ny\n";
        assert_eq!(
            merge_text(base, left, right),
            MergeOutcome::Clean(b"a\nx\nb\ny\n".to_vec())
        );
    }

    #[test]
    fn same_point_inserts_conflict() {
        let base = b"a\n";
        assert_eq!(
            merge_text(base, b"a\nx\n", b"a\ny\n"),
            MergeOutcome::Conflict
        );
    }

    #[test]
    fn binary_is_not_merged() {
        assert_eq!(
            merge_text(b"a\0b", b"a\0c", b"a\0d"),
            MergeOutcome::Conflict
        );
        assert!(!is_mergeable_text(b"a\0b"));
        assert!(is_mergeable_text(b""));
    }
}
