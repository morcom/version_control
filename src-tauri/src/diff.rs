use std::collections::VecDeque;

use similar::{ChangeTag, TextDiff};

use crate::model::DiffRow;

const CONTEXT_LINES: usize = 3;
const MAX_ROWS: usize = 6000;
const MAX_UNIFIED_BYTES: usize = 2 * 1024 * 1024;
const DIFF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

pub struct DiffOutcome {
    pub rows: Vec<DiffRow>,
    pub added: i64,
    pub removed: i64,
    pub unified: String,
    pub truncated: bool,
}

fn clean(value: &str) -> String {
    value
        .trim_end_matches('\n')
        .trim_end_matches('\r')
        .to_string()
}

type PendingLine<'a> = (i64, i64, &'a str);

fn same_row(line: PendingLine<'_>) -> DiffRow {
    DiffRow {
        tag: "same".to_string(),
        old_line: Some(line.0),
        new_line: Some(line.1),
        text: clean(line.2),
    }
}

fn flush_equal(rows: &mut Vec<DiffRow>, pending: &mut VecDeque<PendingLine<'_>>, keep: usize, dropped_front: &mut usize) {
    let extra = std::mem::take(dropped_front);
    let keep = keep.min(pending.len());
    let skipped = pending.len() - keep + extra;
    if skipped > 0 {
        rows.push(DiffRow {
            tag: "skip".to_string(),
            old_line: None,
            new_line: None,
            text: format!("{} unchanged lines", skipped),
        });
    }
    let start = pending.len() - keep;
    rows.extend(pending.drain(..).skip(start).map(same_row));
}

pub struct DiffStats {
    pub added: i64,
    pub removed: i64,
    pub snippet: Option<String>,
}

pub fn diff_stats(old: &str, new: &str, max_snippet_lines: usize) -> DiffStats {
    let diff = TextDiff::configure().timeout(DIFF_TIMEOUT).diff_lines(old, new);
    let mut added = 0i64;
    let mut removed = 0i64;
    let mut snippet = String::new();
    let mut lines = 0usize;
    for change in diff.iter_all_changes() {
        let marker = match change.tag() {
            ChangeTag::Equal => continue,
            ChangeTag::Delete => {
                removed += 1;
                '-'
            }
            ChangeTag::Insert => {
                added += 1;
                '+'
            }
        };
        if lines < max_snippet_lines {
            snippet.push(marker);
            snippet.push(' ');
            snippet.push_str(&clean(change.value()));
            snippet.push('\n');
            lines += 1;
        }
    }
    DiffStats {
        added,
        removed,
        snippet: (!snippet.is_empty()).then_some(snippet),
    }
}

pub fn diff_lines(old: &str, new: &str, old_label: &str, new_label: &str) -> DiffOutcome {
    let diff = TextDiff::configure()
        .timeout(DIFF_TIMEOUT)
        .diff_lines(old, new);
    let mut unified = diff
        .unified_diff()
        .context_radius(CONTEXT_LINES)
        .header(old_label, new_label)
        .to_string();
    if unified.len() > MAX_UNIFIED_BYTES {
        let mut cut = MAX_UNIFIED_BYTES;
        while !unified.is_char_boundary(cut) {
            cut -= 1;
        }
        unified.truncate(cut);
        unified.push_str("\n... output truncated ...\n");
    }

    let mut rows: Vec<DiffRow> = Vec::new();
    let mut pending: VecDeque<PendingLine<'_>> = VecDeque::new();
    let mut added = 0i64;
    let mut removed = 0i64;
    let mut old_line = 0i64;
    let mut new_line = 0i64;
    let mut truncated = false;

    let mut dropped_front = 0usize;

    for change in diff.iter_all_changes() {
        let tag = change.tag();
        match tag {
            ChangeTag::Equal => {
                old_line += 1;
                new_line += 1;
            }
            ChangeTag::Delete => {
                removed += 1;
                old_line += 1;
            }
            ChangeTag::Insert => {
                added += 1;
                new_line += 1;
            }
        }
        if truncated {
            continue;
        }
        match tag {
            ChangeTag::Equal => {
                pending.push_back((old_line, new_line, change.value()));
                if pending.len() > MAX_ROWS {
                    pending.pop_front();
                    dropped_front += 1;
                }
            }
            ChangeTag::Delete | ChangeTag::Insert => {
                flush_equal(&mut rows, &mut pending, CONTEXT_LINES, &mut dropped_front);
                let is_delete = tag == ChangeTag::Delete;
                rows.push(DiffRow {
                    tag: if is_delete { "del" } else { "ins" }.to_string(),
                    old_line: is_delete.then_some(old_line),
                    new_line: (!is_delete).then_some(new_line),
                    text: clean(change.value()),
                });
                if rows.len() >= MAX_ROWS {
                    truncated = true;
                }
            }
        }
    }
    if !truncated {
        let keep = if added == 0 && removed == 0 {
            usize::MAX
        } else {
            CONTEXT_LINES
        };
        flush_equal(&mut rows, &mut pending, keep, &mut dropped_front);
    }

    DiffOutcome {
        rows,
        added,
        removed,
        unified,
        truncated,
    }
}

pub fn snippet(outcome: &DiffOutcome, max_lines: usize) -> Option<String> {
    let mut out = String::new();
    let mut count = 0usize;
    for row in &outcome.rows {
        if count >= max_lines {
            break;
        }
        match row.tag.as_str() {
            "same" => continue,
            "skip" => {
                out.push_str(&format!("... {}\n", row.text));
            }
            "del" => {
                out.push_str(&format!("- {}\n", row.text));
            }
            "ins" => {
                out.push_str(&format!("+ {}\n", row.text));
            }
            _ => {}
        }
        count += 1;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

pub fn binary_snippet(old_size: i64, new_size: i64) -> Option<String> {
    let delta = new_size - old_size;
    Some(format!(
        "binary content changed, {} bytes -> {} bytes ({:+})",
        old_size, new_size, delta
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_added_and_removed_lines() {
        let old = "one\ntwo\nthree\n";
        let new = "one\ntwo changed\nthree\nfour\n";
        let outcome = diff_lines(old, new, "old", "new");
        assert_eq!(outcome.added, 2);
        assert_eq!(outcome.removed, 1);
    }

    #[test]
    fn identical_files_have_no_changes() {
        let text = "same\ncontent\n";
        let outcome = diff_lines(text, text, "old", "new");
        assert_eq!(outcome.added, 0);
        assert_eq!(outcome.removed, 0);
        assert!(outcome.rows.iter().all(|row| row.tag == "same"));
    }

    #[test]
    fn snippet_contains_only_changed_lines() {
        let old = "keep\nremove me\nkeep\n";
        let new = "keep\nkeep\n";
        let outcome = diff_lines(old, new, "old", "new");
        let snippet = snippet(&outcome, 4).unwrap();
        assert!(snippet.contains("- remove me"));
        assert!(!snippet.contains("+ "));
    }

    #[test]
    fn long_unchanged_regions_are_collapsed() {
        let old: String = (0..200).map(|i| format!("line {}\n", i)).collect();
        let new = old.replace("line 100\n", "line 100 changed\n");
        let outcome = diff_lines(&old, &new, "old", "new");
        assert!(outcome.rows.iter().any(|row| row.tag == "skip"));
        assert!(outcome.rows.len() < 200);
    }

    #[test]
    fn unified_output_contains_headers() {
        let outcome = diff_lines("a\n", "b\n", "left.txt", "right.txt");
        assert!(outcome.unified.contains("left.txt"));
        assert!(outcome.unified.contains("right.txt"));
    }

    #[test]
    fn huge_file_with_small_change_is_fast_and_bounded() {
        let old: String = (0..200_000).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 150000\n", "line 150000 changed\n");
        let started = std::time::Instant::now();
        let outcome = diff_lines(&old, &new, "a", "b");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!((outcome.added, outcome.removed), (1, 1));
        assert!(outcome.rows.len() < 20);
        let skip = outcome.rows.iter().find(|r| r.tag == "skip").unwrap();
        assert!(skip.text.starts_with("149997 "), "{}", skip.text);
    }

    #[test]
    fn unchanged_file_keeps_all_rows_up_to_the_cap() {
        let text: String = (0..50).map(|i| format!("l{i}\n")).collect();
        let outcome = diff_lines(&text, &text, "a", "b");
        assert_eq!(outcome.rows.len(), 50);
    }

    #[test]
    fn stats_match_full_diff() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nB\nc\nd\ne\n";
        let stats = diff_stats(old, new, 6);
        let full = diff_lines(old, new, "x", "y");
        assert_eq!((stats.added, stats.removed), (full.added, full.removed));
        let snippet = stats.snippet.unwrap();
        assert!(snippet.contains("- b") && snippet.contains("+ B") && snippet.contains("+ e"));
    }
}

