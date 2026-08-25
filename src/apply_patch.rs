//! Port of Codex's `apply_patch` format (`src/apply-patch.ts`).
//!
//! Parsing and applying are deliberately separate passes: the whole patch is
//! validated and every hunk resolved against the current file contents before a
//! single byte is written, so a patch that fails halfway never leaves the
//! working tree half-edited.

#[derive(Debug, Clone)]
pub struct UpdateChunk {
    /// A single line used to narrow down where the chunk applies (`@@ <text>`).
    pub change_context: Option<String>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    /// Set by `*** End of File`: `old_lines` must sit at the end of the file.
    pub is_end_of_file: bool,
}

#[derive(Debug, Clone)]
pub enum PatchAction {
    Add {
        path: String,
        lines: Vec<String>,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_path: Option<String>,
        chunks: Vec<UpdateChunk>,
    },
}

impl PatchAction {
    pub fn path(&self) -> &str {
        match self {
            PatchAction::Add { path, .. }
            | PatchAction::Delete { path }
            | PatchAction::Update { path, .. } => path,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PatchParseError(pub String);

/// An apply-time failure (a hunk that does not match the file). Distinct from a
/// parse error so the tool can report "Patch does not apply" vs "Invalid patch".
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ApplyError(pub String);

const BEGIN_PATCH: &str = "*** Begin Patch";
const END_PATCH: &str = "*** End Patch";
const ADD_FILE: &str = "*** Add File: ";
const DELETE_FILE: &str = "*** Delete File: ";
const UPDATE_FILE: &str = "*** Update File: ";
const MOVE_TO: &str = "*** Move to: ";
const END_OF_FILE: &str = "*** End of File";

fn is_hunk_start(line: &str) -> bool {
    line.starts_with(ADD_FILE)
        || line.starts_with(DELETE_FILE)
        || line.starts_with(UPDATE_FILE)
        || line == END_PATCH
}

pub fn parse_patch(patch: &str) -> Result<Vec<PatchAction>, PatchParseError> {
    let normalized = patch.replace("\r\n", "\n");
    let mut lines: Vec<&str> = normalized.split('\n').collect();

    // Tolerate leading/trailing blank lines around the envelope; models routinely
    // add them when the patch is embedded in JSON.
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }

    if lines.first() != Some(&BEGIN_PATCH) {
        return Err(PatchParseError(format!(
            "The first line of the patch must be '{BEGIN_PATCH}'"
        )));
    }
    if lines.last() != Some(&END_PATCH) {
        return Err(PatchParseError(format!(
            "The last line of the patch must be '{END_PATCH}'"
        )));
    }

    let mut actions: Vec<PatchAction> = Vec::new();
    let mut i = 1usize;

    while i < lines.len() {
        let line = lines[i];

        if line == END_PATCH {
            i += 1;
            continue;
        }

        if let Some(path) = line.strip_prefix(ADD_FILE) {
            let path = path.to_string();
            i += 1;
            let mut added: Vec<String> = Vec::new();
            while i < lines.len() && !is_hunk_start(lines[i]) {
                let body = lines[i];
                let Some(rest) = body.strip_prefix('+') else {
                    return Err(PatchParseError(format!(
                        "Unexpected line found in add hunk: '{body}'. Every line should start with '+'"
                    )));
                };
                added.push(rest.to_string());
                i += 1;
            }
            actions.push(PatchAction::Add { path, lines: added });
            continue;
        }

        if let Some(path) = line.strip_prefix(DELETE_FILE) {
            actions.push(PatchAction::Delete {
                path: path.to_string(),
            });
            i += 1;
            continue;
        }

        if let Some(path) = line.strip_prefix(UPDATE_FILE) {
            let path = path.to_string();
            i += 1;

            let mut move_path: Option<String> = None;
            if i < lines.len()
                && let Some(dest) = lines[i].strip_prefix(MOVE_TO)
            {
                move_path = Some(dest.to_string());
                i += 1;
            }

            let mut chunks: Vec<UpdateChunk> = Vec::new();
            let mut have_chunk = false;

            while i < lines.len() && !is_hunk_start(lines[i]) {
                let body = lines[i];

                if let Some(ctx) = body.strip_prefix("@@") {
                    let ctx = ctx.trim();
                    chunks.push(UpdateChunk {
                        change_context: if ctx.is_empty() {
                            None
                        } else {
                            Some(ctx.to_string())
                        },
                        old_lines: Vec::new(),
                        new_lines: Vec::new(),
                        is_end_of_file: false,
                    });
                    have_chunk = true;
                    i += 1;
                    continue;
                }

                if !have_chunk {
                    // Codex's parser is lenient here: an update hunk whose body
                    // begins before any `@@` header is treated as a single
                    // context-less chunk. Synthesize one rather than rejecting.
                    chunks.push(UpdateChunk {
                        change_context: None,
                        old_lines: Vec::new(),
                        new_lines: Vec::new(),
                        is_end_of_file: false,
                    });
                    have_chunk = true;
                }
                let chunk = chunks.last_mut().unwrap();

                if body == END_OF_FILE {
                    chunk.is_end_of_file = true;
                    i += 1;
                    continue;
                }

                if let Some(rest) = body.strip_prefix('+') {
                    chunk.new_lines.push(rest.to_string());
                } else if let Some(rest) = body.strip_prefix('-') {
                    chunk.old_lines.push(rest.to_string());
                } else if body.is_empty() || body.starts_with(' ') {
                    // A bare empty line is a context line whose leading space was
                    // trimmed.
                    let text = if body.is_empty() { "" } else { &body[1..] };
                    chunk.old_lines.push(text.to_string());
                    chunk.new_lines.push(text.to_string());
                } else {
                    return Err(PatchParseError(format!(
                        "Unexpected line found in update hunk: '{body}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                    )));
                }
                i += 1;
            }

            if chunks.is_empty() {
                return Err(PatchParseError(format!(
                    "Update hunk for '{path}' contains no @@ chunks"
                )));
            }
            actions.push(PatchAction::Update {
                path,
                move_path,
                chunks,
            });
            continue;
        }

        return Err(PatchParseError(format!(
            "Unexpected line found in patch: '{line}'. Expected '{}', '{}', '{}' or '{END_PATCH}'",
            ADD_FILE.trim(),
            DELETE_FILE.trim(),
            UPDATE_FILE.trim()
        )));
    }

    if actions.is_empty() {
        return Err(PatchParseError("The patch contains no hunks".to_string()));
    }
    Ok(actions)
}

/// Normalises typographic punctuation to ASCII so a patch authored with plain
/// quotes still matches source that contains curly ones — the same leniency
/// `git apply` has when locating context.
fn normalise_fuzzy(text: &str) -> String {
    text.trim()
        .chars()
        .map(|c| match c {
            // U+2010..U+2015 and U+2212 (dashes)
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            // U+2018..U+201B (curly single quotes)
            '\u{2018}'..='\u{201B}' => '\'',
            // U+201C..U+201F (curly double quotes)
            '\u{201C}'..='\u{201F}' => '"',
            // Assorted non-breaking and typographic spaces.
            '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// Finds `pattern` within `lines` at or after `start`, trying progressively
/// looser comparisons: exact, ignoring trailing whitespace, ignoring whitespace
/// on both sides, then with punctuation normalised. When `eof` is set the search
/// begins at the last position where the pattern could still fit.
pub fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }

    let last = lines.len() - pattern.len();
    let search_start = if eof { last } else { start };

    type Cmp = fn(&str, &str) -> bool;
    let passes: [Cmp; 4] = [
        |a, b| a == b,
        |a, b| a.trim_end() == b.trim_end(),
        |a, b| a.trim() == b.trim(),
        |a, b| normalise_fuzzy(a) == normalise_fuzzy(b),
    ];

    for matches in passes {
        let mut i = search_start;
        while i <= last {
            let mut ok = true;
            for p in 0..pattern.len() {
                if !matches(&lines[i + p], &pattern[p]) {
                    ok = false;
                    break;
                }
            }
            if ok {
                return Some(i);
            }
            i += 1;
        }
    }
    None
}

struct Replacement {
    start: usize,
    old_len: usize,
    new_lines: Vec<String>,
}

fn compute_replacements(
    original_lines: &[String],
    path: &str,
    chunks: &[UpdateChunk],
) -> Result<Vec<Replacement>, ApplyError> {
    let mut replacements: Vec<Replacement> = Vec::new();
    let mut line_index = 0usize;

    for chunk in chunks {
        if let Some(ctx) = &chunk.change_context {
            let ctx_pat = vec![ctx.clone()];
            let idx = seek_sequence(original_lines, &ctx_pat, line_index, false)
                .ok_or_else(|| ApplyError(format!("Failed to find context '{ctx}' in {path}")))?;
            line_index = idx + 1;
        }

        if chunk.old_lines.is_empty() {
            // Pure insertion: append at the end of the file.
            replacements.push(Replacement {
                start: original_lines.len(),
                old_len: 0,
                new_lines: chunk.new_lines.clone(),
            });
            continue;
        }

        let mut pattern: Vec<String> = chunk.old_lines.clone();
        let mut new_slice: Vec<String> = chunk.new_lines.clone();
        let mut found = seek_sequence(original_lines, &pattern, line_index, chunk.is_end_of_file);

        if found.is_none() && pattern.last().map(|s| s.as_str()) == Some("") {
            // The trailing empty string stands for the file's final newline, which
            // is not a real line. Retry without it.
            pattern.pop();
            if new_slice.last().map(|s| s.as_str()) == Some("") {
                new_slice.pop();
            }
            found = seek_sequence(original_lines, &pattern, line_index, chunk.is_end_of_file);
        }

        let found = found.ok_or_else(|| {
            ApplyError(format!(
                "Failed to find expected lines in {path}:\n{}",
                chunk.old_lines.join("\n")
            ))
        })?;

        let len = pattern.len();
        replacements.push(Replacement {
            start: found,
            old_len: len,
            new_lines: new_slice,
        });
        line_index = found + len;
    }

    replacements.sort_by_key(|r| r.start);
    Ok(replacements)
}

fn apply_replacements(lines: &[String], replacements: &[Replacement]) -> Vec<String> {
    let mut result: Vec<String> = lines.to_vec();
    // Descending order, so earlier splices do not shift later indices.
    for r in replacements.iter().rev() {
        let end = r.start + r.old_len;
        result.splice(r.start..end, r.new_lines.iter().cloned());
    }
    result
}

/// True when the file predominantly uses CRLF line endings.
pub fn uses_crlf(contents: &str) -> bool {
    let crlf = contents.matches("\r\n").count();
    if crlf == 0 {
        return false;
    }
    let lf = contents.matches('\n').count();
    crlf * 2 >= lf
}

/// Returns the new contents of a file after applying `chunks`. Line endings are
/// matched to whatever the file already used.
pub fn apply_update(
    original_contents: &str,
    chunks: &[UpdateChunk],
    path: &str,
) -> Result<String, ApplyError> {
    let crlf = uses_crlf(original_contents);
    let normalized = original_contents.replace("\r\n", "\n");

    let mut original_lines: Vec<String> = normalized.split('\n').map(String::from).collect();
    // Drop the empty element produced by a trailing newline so line counts match
    // what `diff` would report.
    if original_lines.last().map(|s| s.as_str()) == Some("") {
        original_lines.pop();
    }

    let replacements = compute_replacements(&original_lines, path, chunks)?;
    let mut new_lines = apply_replacements(&original_lines, &replacements);
    new_lines.push(String::new());

    let joined = new_lines.join("\n");
    Ok(if crlf {
        joined.replace('\n', "\r\n")
    } else {
        joined
    })
}

/// Renders the body of an `*** Add File:` hunk into file contents.
pub fn render_added_file(lines: &[String]) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_add_file() {
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+hello\n+world\n*** End Patch";
        let actions = parse_patch(patch).unwrap();
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            PatchAction::Add { path, lines } => {
                assert_eq!(path, "a.txt");
                assert_eq!(lines, &["hello", "world"]);
            }
            _ => panic!("expected add"),
        }
    }

    #[test]
    fn tolerates_blank_envelope() {
        let patch = "\n\n*** Begin Patch\n*** Delete File: x\n*** End Patch\n\n";
        assert!(parse_patch(patch).is_ok());
    }

    #[test]
    fn rejects_missing_begin() {
        assert!(parse_patch("*** Add File: a\n+x\n*** End Patch").is_err());
    }

    #[test]
    fn applies_update() {
        let original = "one\ntwo\nthree\n";
        let patch =
            "*** Begin Patch\n*** Update File: f\n@@\n one\n-two\n+TWO\n three\n*** End Patch";
        let actions = parse_patch(patch).unwrap();
        let PatchAction::Update { chunks, .. } = &actions[0] else {
            panic!("expected update");
        };
        let out = apply_update(original, chunks, "f").unwrap();
        assert_eq!(out, "one\nTWO\nthree\n");
    }

    #[test]
    fn update_hunk_without_leading_context_marker() {
        // Codex accepts an update body that starts before any `@@` header; a
        // single context-less chunk is synthesized.
        let original = "one\ntwo\nthree\n";
        let patch = "*** Begin Patch\n*** Update File: f\n one\n-two\n+TWO\n three\n*** End Patch";
        let actions = parse_patch(patch).unwrap();
        let PatchAction::Update { chunks, .. } = &actions[0] else {
            panic!("expected update");
        };
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].change_context.is_none());
        let out = apply_update(original, chunks, "f").unwrap();
        assert_eq!(out, "one\nTWO\nthree\n");
    }

    #[test]
    fn fuzzy_matches_curly_quotes() {
        let original = "let s = \u{201C}hi\u{201D};\n";
        let patch = "*** Begin Patch\n*** Update File: f\n@@\n-let s = \"hi\";\n+let s = \"bye\";\n*** End Patch";
        let actions = parse_patch(patch).unwrap();
        let PatchAction::Update { chunks, .. } = &actions[0] else {
            panic!("expected update");
        };
        let out = apply_update(original, chunks, "f").unwrap();
        assert_eq!(out, "let s = \"bye\";\n");
    }

    #[test]
    fn preserves_crlf() {
        let original = "a\r\nb\r\n";
        let patch = "*** Begin Patch\n*** Update File: f\n@@\n a\n-b\n+B\n*** End Patch";
        let actions = parse_patch(patch).unwrap();
        let PatchAction::Update { chunks, .. } = &actions[0] else {
            panic!("expected update");
        };
        let out = apply_update(original, chunks, "f").unwrap();
        assert_eq!(out, "a\r\nB\r\n");
    }

    #[test]
    fn render_added_file_trailing_newline() {
        assert_eq!(render_added_file(&["x".to_string()]), "x\n");
        assert_eq!(render_added_file(&[]), "");
    }
}
