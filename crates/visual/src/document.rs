//! Visuals embedded in Markdown documents as ```` ```zeron-visual ```` fences.
//!
//! Zeron's Markdown preview runs these fences with its full parser; this is
//! the light line scanner `zeron visual check` uses to find them, so an agent
//! can check a document's visuals and map each error back to a line of the
//! `.md` without a running Zeron.

/// The fence language that marks a visual (case-insensitive).
pub const FENCE_LANGUAGE: &str = "zeron-visual";

pub fn is_fence_language(language: &str) -> bool {
    language.eq_ignore_ascii_case(FENCE_LANGUAGE)
}

/// One visual fence in a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fence {
    /// The 1-based line of the opening fence: script line `n` sits on
    /// document line `line + n`.
    pub line: usize,
    pub code: String,
    /// Its position among fences with identical code (see
    /// [`crate::seed_for_document`]).
    pub occurrence: usize,
}

struct Open {
    marker: char,
    len: usize,
    /// What precedes the fence on its line: list indentation and quote markers.
    prefix: String,
    /// The fence's own indentation, stripped from its content lines.
    indent: usize,
    visual: Option<Fence>,
}

/// Every visual fence in `markdown`, in document order. Other fences are
/// skipped whole, so a visual quoted inside a ```` ```markdown ```` fence is
/// not one.
pub fn fences(markdown: &str) -> Vec<Fence> {
    let mut out: Vec<Fence> = Vec::new();
    let mut open: Option<Open> = None;
    let finish = |fence: Open, out: &mut Vec<Fence>| {
        if let Some(mut visual) = fence.visual {
            visual.occurrence = out.iter().filter(|f| f.code == visual.code).count();
            out.push(visual);
        }
    };
    for (ix, raw) in markdown.lines().enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(fence) = &mut open {
            match strip_container(line, &fence.prefix) {
                Some(content) if !closes(content, fence.marker, fence.len) => {
                    if let Some(visual) = &mut fence.visual {
                        // `ix` is the 0-based index, so the first content
                        // line has `ix == visual.line`.
                        if ix > visual.line {
                            visual.code.push('\n');
                        }
                        visual.code.push_str(strip_spaces(content, fence.indent));
                    }
                    continue;
                }
                content => {
                    finish(open.take().expect("open fence"), &mut out);
                    if content.is_some() {
                        continue;
                    }
                    // The quote holding the fence ended, so this line may
                    // open a fence of its own.
                }
            }
        }
        open = opening(line, ix + 1);
    }
    if let Some(fence) = open {
        finish(fence, &mut out);
    }
    out
}

/// A fence opened by `line`, if it opens one.
fn opening(line: &str, number: usize) -> Option<Open> {
    let body_start = container_len(line);
    let (prefix, body) = line.split_at(body_start);
    let indent = body.len() - body.trim_start_matches(' ').len();
    let body = &body[indent..];
    let marker = body.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let len = body.chars().take_while(|c| *c == marker).count();
    if len < 3 {
        return None;
    }
    let info = body[len..].trim();
    if marker == '`' && info.contains('`') {
        return None;
    }
    let language = info.split_whitespace().next().unwrap_or("");
    Some(Open {
        marker,
        len,
        prefix: prefix.to_owned(),
        indent,
        visual: is_fence_language(language).then(|| Fence {
            // Content starts on the next line; `line + n` is script line n.
            line: number,
            code: String::new(),
            occurrence: 0,
        }),
    })
}

/// The length of a line's container markers: list bullets and numbers, quote
/// markers, and the spaces between them, up to where the block itself starts.
fn container_len(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut ix = 0;
    loop {
        let start = ix;
        while ix < bytes.len() && bytes[ix] == b' ' {
            ix += 1;
        }
        match bytes.get(ix) {
            Some(b'>') => {
                ix += 1;
                if bytes.get(ix) == Some(&b' ') {
                    ix += 1;
                }
            }
            Some(b'-' | b'*' | b'+') if bytes.get(ix + 1) == Some(&b' ') => ix += 2,
            Some(b'0'..=b'9') => {
                let digits = bytes[ix..]
                    .iter()
                    .take_while(|b| b.is_ascii_digit())
                    .count();
                match (bytes.get(ix + digits), bytes.get(ix + digits + 1)) {
                    (Some(b'.' | b')'), Some(b' ')) => ix += digits + 2,
                    _ => return start,
                }
            }
            _ => return start,
        }
    }
}

/// A content line with the opening line's container removed: its quote
/// markers must be there (`None` when the quote ended); list indentation is
/// removed where present.
fn strip_container<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let mut rest = line;
    let mut prefix = prefix;
    while !prefix.is_empty() {
        let spaces = prefix.len() - prefix.trim_start_matches(' ').len();
        prefix = &prefix[spaces..];
        match prefix.chars().next() {
            Some('>') => {
                rest = rest.trim_start_matches(' ').strip_prefix('>')?;
                rest = rest.strip_prefix(' ').unwrap_or(rest);
                prefix = prefix[1..].strip_prefix(' ').unwrap_or(&prefix[1..]);
            }
            Some(_) => {
                // A list marker: its content lines are indented by its width.
                let marker = prefix.find(' ').map_or(prefix.len(), |at| at + 1);
                rest = strip_spaces(rest, spaces + marker);
                prefix = &prefix[marker..];
            }
            None => rest = strip_spaces(rest, spaces),
        }
    }
    Some(rest)
}

fn strip_spaces(line: &str, max: usize) -> &str {
    let spaces = line.len() - line.trim_start_matches(' ').len();
    &line[spaces.min(max)..]
}

fn closes(content: &str, marker: char, len: usize) -> bool {
    let body = content.trim_start_matches(' ');
    let run = body.chars().take_while(|c| *c == marker).count();
    content.len() - body.len() <= 3 && run >= len && body[run..].trim().is_empty()
}

/// Where a script error points in the document: the line of its first
/// `visual:N:` position (the fence line when it has none) and the message
/// without that position. A Luau traceback only lends its position: the
/// message stops where the traceback starts.
pub fn locate(message: &str, fence_line: usize) -> (usize, String) {
    let (head, traceback) = message
        .split_once("\nstack traceback:")
        .unwrap_or((message, ""));
    if let Some((at, len, line)) = position(head) {
        let after = head[at + len..].trim_start();
        return (fence_line + line, format!("{}{after}", &head[..at]));
    }
    let line = position(traceback).map_or(0, |(_, _, line)| line);
    (fence_line + line, head.trim_end().to_owned())
}

/// The first `visual:N:` in `text`: its offset, length, and N.
fn position(text: &str) -> Option<(usize, usize, usize)> {
    let mut search = 0;
    while let Some(found) = text[search..].find("visual:") {
        let at = search + found;
        let rest = &text[at + "visual:".len()..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits > 0
            && rest[digits..].starts_with(':')
            && let Ok(line) = rest[..digits].parse::<usize>()
        {
            return Some((at, "visual:".len() + digits + 1, line));
        }
        search = at + "visual:".len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_backtick_and_tilde_fences_with_their_lines() {
        let doc = "# Doc\n\n```zeron-visual\nlocal a = 1\nreturn a\n```\n\ntext\n\n~~~ Zeron-Visual extra\nline 1\n~~~\n";
        let found = fences(doc);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].line, 3);
        assert_eq!(found[0].code, "local a = 1\nreturn a");
        assert_eq!(found[1].line, 10);
        assert_eq!(found[1].code, "line 1");
    }

    #[test]
    fn other_fences_hide_what_they_contain() {
        let doc = "````markdown\n```zeron-visual\nnot a visual\n```\n````\n```lua\nx\n```\n";
        assert!(fences(doc).is_empty());
    }

    #[test]
    fn nested_fences_lose_their_container_indentation() {
        let doc = "- item\n\n  ```zeron-visual\n  local a = 1\n    nested()\n  ```\n\n> ```zeron-visual\n> quoted()\n> ```\n";
        let found = fences(doc);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].line, 3);
        assert_eq!(found[0].code, "local a = 1\n  nested()");
        assert_eq!(found[1].line, 8);
        assert_eq!(found[1].code, "quoted()");
    }

    #[test]
    fn list_markers_on_the_fence_line_are_containers() {
        let doc = "1. ```zeron-visual\n   one()\n   ```\n";
        let found = fences(doc);
        assert_eq!(found[0].code, "one()");
    }

    #[test]
    fn crlf_blank_lines_and_unclosed_fences() {
        let doc = "```zeron-visual\r\n\r\nfirst()\r\n```\r\n```zeron-visual\nlast()";
        let found = fences(doc);
        assert_eq!(found[0].code, "\nfirst()");
        assert_eq!(found[1].code, "last()");
        assert_eq!(found[1].line, 5);
    }

    #[test]
    fn identical_fences_count_occurrences() {
        let doc = "```zeron-visual\nx\n```\n```zeron-visual\ny\n```\n```zeron-visual\nx\n```\n";
        let found = fences(doc);
        let occurrences: Vec<usize> = found.iter().map(|f| f.occurrence).collect();
        assert_eq!(occurrences, [0, 0, 1]);
    }

    #[test]
    fn errors_map_to_document_lines() {
        assert_eq!(
            locate("visual:3: attempt to call a nil value", 10),
            (13, "attempt to call a nil value".into())
        );
        assert_eq!(
            locate("after selecting tab 'b': visual:5: boom", 2),
            (7, "after selecting tab 'b': boom".into())
        );
        assert_eq!(
            locate("the script used more than 64 MB", 4),
            (4, "the script used more than 64 MB".into())
        );
        assert_eq!(locate("visual:x: odd", 1), (1, "visual:x: odd".into()));
        assert_eq!(
            locate(
                "attempt to call a nil value\nstack traceback:\n\t[C]: in ?\n\tvisual:4: in function <visual:1>",
                31
            ),
            (35, "attempt to call a nil value".into())
        );
    }
}
