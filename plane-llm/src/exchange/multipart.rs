//! Rung 2 of the model ladder: the `model` form field of a multipart arrival, read as a document. The
//! arrival reading is the plane's, and there is one of it.

/// The first occurrence of `needle` in `hay`.
fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The boundary, off the `Content-Type`: the one thing that says where a part begins. Every
/// delimiter in the body is spelled from it, so a reader without it reads bytes that resemble parts.
/// `None` where the header has no `boundary` parameter (a body no conforming parser, the provider's
/// included, can read).
///
/// The parameter is matched as a whole key, not by substring, so a `boundary` inside another
/// parameter's quoted value is not mistaken for it; the value may be quoted and is unquoted (RFC 2045).
fn multipart_boundary(content_type: &str) -> Option<String> {
    for param in split_params(content_type).skip(1) {
        let (key, value) = param.split_once('=')?;
        if key.trim().eq_ignore_ascii_case("boundary") {
            let v = value.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|r| r.strip_suffix('"'))
                .unwrap_or(v);
            return (!v.is_empty()).then(|| v.to_string());
        }
    }
    None
}

/// Split a header value on `;`, ignoring separators inside a quoted string (a naive `split(';')` cuts
/// `filename="a;b"` in half and invents a parameter). The first item is the value's own token
/// (`multipart/form-data`), which the boundary search skips.
fn split_params(value: &str) -> impl Iterator<Item = &str> {
    let mut out = Vec::new();
    let (mut start, mut quoted) = (0usize, false);
    for (i, c) in value.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => {
                out.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&value[start..]);
    out.into_iter()
}

/// Whether a part's header block declares the form field `model`: the `name` parameter of
/// `Content-Disposition` matched as a whole key against a whole value, never a substring of the block.
/// `filename="model"` is a different parameter; a `name` inside another part's body is not a header
/// and never reaches here (the caller hands over only bytes the boundary said were headers).
fn part_is_model(headers: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(headers) else {
        // A header block is ASCII by construction. One that is not is not a header block this
        // reader will guess at.
        return false;
    };
    // Header field lines are unfolded here only as far as the split: a folded `Content-Disposition`
    // is not something any client emits for a form part, and a continuation line that failed to
    // match simply makes this part not-the-model, which is the safe direction.
    for line in text.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("content-disposition") {
            continue;
        }
        for param in split_params(value).skip(1) {
            let Some((key, v)) = param.split_once('=') else {
                continue;
            };
            if !key.trim().eq_ignore_ascii_case("name") {
                continue;
            }
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|r| r.strip_suffix('"'))
                .unwrap_or(v);
            return v == "model";
        }
    }
    false
}

/// Rung 2 of the model ladder: the `model` form field of a multipart arrival, read as a document.
///
/// This model decides the lane the unit is verified against, the pool it is admitted to and the card
/// that prices it, while the provider decides which model answers by parsing the same bytes with a
/// conforming multipart parser. The two readings must agree or the caller picks what busbar charges
/// independently of what it receives, so this walks the parts the boundary delimits and reads the
/// `model` part's headers, rather than scanning for a string that occurs as readily in a prompt.
///
/// Two `model` parts resolve to nothing: repetition is legal and no rule says which one a provider
/// takes, so busbar cannot know the model. It falls to the ladder's floor and gets the missing-model
/// refusal, before the door, costing the caller nothing.
///
/// The 64 KiB bound stays: far larger than any run of text form fields before the audio blob, and it
/// stops a walk of megabytes of binary looking for a delimiter. A `model` part beyond it is not found,
/// the same safe direction (a refusal, never a different model).
///
/// Public so every reader of the arrival calls this rung rather than carrying a second copy: two
/// readings of one wire are two answers waiting to disagree.
pub fn multipart_model(content_type: &str, body: &[u8]) -> Option<String> {
    const HEAD: usize = 64 * 1024;
    let boundary = multipart_boundary(content_type)?;
    let head = &body[..body.len().min(HEAD)];

    // Every part after the first is opened by CRLF + `--boundary`; the first by `--boundary` at the
    // body's start (an optional preamble may precede it, then it too is CRLF-prefixed). Both are one
    // delimiter with an optional CRLF in front, so the walk searches the dashed form and accepts it
    // only at offset zero or right after a CRLF.
    let dashed = format!("--{boundary}");
    let closing = format!("\r\n--{boundary}");
    let mut found: Option<String> = None;
    let mut at = 0usize;

    while at < head.len() {
        let Some(rel) = find_sub(&head[at..], dashed.as_bytes()) else {
            break;
        };
        let abs = at + rel;
        // A `--boundary` that is not at a line start is text that happens to look like a delimiter.
        if abs != 0 && !head[..abs].ends_with(b"\r\n") {
            at = abs + dashed.len();
            continue;
        }
        let after = abs + dashed.len();
        let rest = &head[after..];
        if rest.starts_with(b"--") {
            // The closing delimiter. Everything past it is the epilogue.
            break;
        }
        // A delimiter line may carry linear whitespace before its CRLF; anything else is not a
        // delimiter line, and a document this reader cannot follow is a document it does not guess
        // at.
        let Some(line_end) = find_sub(rest, b"\r\n") else {
            break;
        };
        if rest[..line_end].iter().any(|b| !matches!(b, b' ' | b'\t')) {
            at = after;
            continue;
        }
        let hdr_at = after + line_end + 2;
        // The part's header block ends at the blank line. A part whose block does not close inside
        // the head is a part this read does not reach.
        let Some(hdr_len) = find_sub(&head[hdr_at..], b"\r\n\r\n") else {
            break;
        };
        let body_at = hdr_at + hdr_len + 4;
        let Some(body_len) = find_sub(&head[body_at..], closing.as_bytes()) else {
            break;
        };
        if part_is_model(&head[hdr_at..hdr_at + hdr_len]) {
            if found.is_some() {
                // Two `model` parts: no model, rather than a guess at which one is billed.
                return None;
            }
            found = Some(
                String::from_utf8_lossy(&head[body_at..body_at + body_len])
                    .trim()
                    .to_string(),
            );
        }
        at = body_at + body_len + 2;
    }
    found.filter(|m| !m.is_empty())
}
