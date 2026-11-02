//! Rung 2 of the model ladder: the `model` form field of a multipart arrival, read as a document. The
//! arrival reading is the plane's, and there is one of it.

use std::borrow::Cow;

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
    parameter(content_type, "boundary")
        .filter(|value| !value.is_empty())
        .map(Cow::into_owned)
}

/// Split a header value on `;`, ignoring separators inside a quoted string (a naive `split(';')` cuts
/// `filename="a;b"` in half and invents a parameter). The first item is the value's own token
/// (`multipart/form-data`), which the boundary search skips.
fn split_params(value: &str) -> impl Iterator<Item = &str> {
    let (mut quoted, mut escaped) = (false, false);
    value.split(move |c| {
        if escaped {
            escaped = false;
            return false;
        }
        match c {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => return true,
            _ => {}
        }
        false
    })
}

fn parameter_value(value: &str) -> Option<Cow<'_, str>> {
    let value = value.trim();
    if !value.starts_with('"') {
        let token = !value.is_empty()
            && value
                .bytes()
                .all(|b| b.is_ascii_graphic() && !b"()<>@,;:\\\"/[]?=".contains(&b));
        return token.then_some(Cow::Borrowed(value));
    }
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    if !inner.contains('\\') {
        return (!inner.contains('"') && !inner.chars().any(|c| c.is_control() && c != '\t'))
            .then_some(Cow::Borrowed(inner));
    }
    let mut decoded = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        let c = match c {
            '\\' => chars.next()?,
            '"' => return None,
            c => c,
        };
        if c.is_control() && c != '\t' {
            return None;
        }
        decoded.push(c);
    }
    Some(Cow::Owned(decoded))
}

fn parameter<'a>(value: &'a str, attribute: &str) -> Option<Cow<'a, str>> {
    let mut found = None;
    for param in split_params(value).skip(1) {
        let Some((key, value)) = param.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case(attribute) {
            if found.is_some() {
                return None;
            }
            found = Some(parameter_value(value)?);
        }
    }
    found
}

/// Only the whole `name` parameter of Content-Disposition names a field. A filename, quoted
/// semicolon or another header cannot supply it. Admission and transcription share this reading.
pub(crate) fn multipart_field_name(headers: &str) -> Option<Cow<'_, str>> {
    for line in headers.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-disposition") {
            return parameter(value, "name");
        }
    }
    None
}

/// Borrowed multipart segments, excluding delimiter lines but retaining the CRLF before each
/// delimiter. The complete upload is already available at arrival; walking it requires no audio
/// copy or per-part allocation. Admission and transcription decoding use the same framing.
pub(crate) struct MultipartSegments<'a> {
    body: &'a [u8],
    delimiter: String,
    next: Option<usize>,
    closed: bool,
}

impl MultipartSegments<'_> {
    fn delimiter_after(&self, mut at: usize) -> Option<(usize, usize, bool)> {
        while at < self.body.len() {
            let start = at + find_sub(&self.body[at..], self.delimiter.as_bytes())?;
            at = start + self.delimiter.len();
            if start != 0 && !self.body[..start].ends_with(b"\r\n") {
                continue;
            }
            let closing = self.body[at..].starts_with(b"--");
            let mut end = at + if closing { 2 } else { 0 };
            while self
                .body
                .get(end)
                .is_some_and(|b| matches!(b, b' ' | b'\t'))
            {
                end += 1;
            }
            if self.body[end..].starts_with(b"\r\n") {
                return Some((start, end + 2, closing));
            }
            if closing && end == self.body.len() {
                return Some((start, end, true));
            }
            // A prefix such as `--boundarySuffix` or `--boundary--Suffix` is payload.
        }
        None
    }
}

impl<'a> Iterator for MultipartSegments<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.next.take()?;
        let (end, after, closing) = self.delimiter_after(start)?;
        self.closed = closing;
        self.next = (!closing).then_some(after);
        Some(&self.body[start..end])
    }
}

pub(crate) fn multipart_segments<'a>(
    content_type: &str,
    body: &'a [u8],
) -> Option<MultipartSegments<'a>> {
    let boundary = multipart_boundary(content_type)?;
    let mut parts = MultipartSegments {
        body,
        delimiter: format!("--{boundary}"),
        next: None,
        closed: false,
    };
    let (_, after, closing) = parts.delimiter_after(0)?;
    parts.closed = closing;
    parts.next = (!closing).then_some(after);
    Some(parts)
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
/// The complete form must be checked: a duplicate model after a large audio part is just as
/// ambiguous as an adjacent duplicate. Only the selected text value is copied, never the audio.
///
/// Public so every reader of the arrival calls this rung rather than carrying a second copy: two
/// readings of one wire are two answers waiting to disagree.
pub fn multipart_model(content_type: &str, body: &[u8]) -> Option<String> {
    multipart_field_value(content_type, body, "model", true)
        .map(|value| String::from_utf8_lossy(value).trim().to_string())
        .filter(|m| !m.is_empty())
}

/// The last form field, as the transcription reader resolves repeated format fields, borrowed
/// from the same complete framing admission uses. This is
/// also how reply formatting retains a transcription's small `response_format` without parsing
/// or copying its uploaded audio.
pub(crate) fn multipart_field<'a>(
    content_type: &str,
    body: &'a [u8],
    name: &str,
) -> Option<&'a [u8]> {
    transcription_fields(content_type, body)
        .into_iter()
        .filter(|field| field.name == name)
        .next_back()
        .map(|field| field.value)
}

/// The bounded named fields used by transcription decoding and its response-format echo.
pub(crate) struct TranscriptionField<'a> {
    pub name: String,
    pub content_type: Option<String>,
    pub value: &'a [u8],
}

pub(crate) fn transcription_fields<'a>(
    content_type: &str,
    body: &'a [u8],
) -> Vec<TranscriptionField<'a>> {
    let Some(parts) = multipart_segments(content_type, body) else {
        return Vec::new();
    };
    // Preserve the transcription reader's existing bound and last-field resolution policy.
    const MAX_MULTIPART_PARTS: usize = 64;
    parts
        .filter_map(|segment| {
            let segment = segment.strip_prefix(b"\r\n").unwrap_or(segment);
            let hpos = find_sub(segment, b"\r\n\r\n")?;
            let headers = String::from_utf8_lossy(&segment[..hpos]);
            let name = multipart_field_name(&headers)?.into_owned();
            let value = &segment[hpos + 4..];
            let value = value.strip_suffix(b"\r\n").unwrap_or(value);
            let content_type = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-type")
                    .then(|| value.trim().to_string())
            });
            Some(TranscriptionField {
                name,
                content_type,
                value,
            })
        })
        .take(MAX_MULTIPART_PARTS)
        .collect()
}

fn multipart_field_value<'a>(
    content_type: &str,
    body: &'a [u8],
    name: &str,
    unique: bool,
) -> Option<&'a [u8]> {
    let mut parts = multipart_segments(content_type, body)?;
    let mut found: Option<&[u8]> = None;
    for part in parts.by_ref() {
        let hdr_len = find_sub(part, b"\r\n\r\n")?;
        let headers = std::str::from_utf8(&part[..hdr_len]).ok()?;
        if multipart_field_name(headers)? == name {
            if unique && found.is_some() {
                // Two `model` parts: no model, rather than a guess at which one is billed.
                return None;
            }
            let value = &part[hdr_len + 4..];
            found = Some(value.strip_suffix(b"\r\n").unwrap_or(value));
        }
    }
    if !parts.closed {
        return None;
    }
    found
}
