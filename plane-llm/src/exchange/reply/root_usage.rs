// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! A bounded, validating projection of billing members from the original JSON root.
//!
//! Every byte is walked from the opening root object, including discarded payloads. A fixed
//! nesting limit and bounded scalar/capture buffers make memory independent of response size.

use serde_json::{Map, Value};

#[derive(Clone, Copy, PartialEq)]
enum Frame {
    ObjectKey(bool),
    Colon,
    ObjectValue,
    ObjectEnd,
    ArrayValue(bool),
    ArrayEnd,
}

/// Incremental root-member custody, with at most eight `cap`-sized retained values.
pub(super) struct RootUsage {
    cap: usize,
    stack: Vec<Frame>,
    started: bool,
    done: bool,
    bad: bool,
    string: bool,
    key: bool,
    key_bytes: Vec<u8>,
    scalar: Vec<u8>,
    character: Vec<u8>,
    name: Option<String>,
    capture: Vec<u8>,
    capturing: bool,
    oversized: bool,
    values: Map<String, Value>,
    // Counts come from validated original arrays, never from a synthetic wire member.
    array_key: Option<usize>,
    arrays: [Option<(u32, bool)>; 3],
    image_usage_present: [bool; 2],
    item_object: bool,
    item_string: bool,
    item_fields: [bool; 3],
    item_key: Option<usize>,
}

impl RootUsage {
    pub(super) fn new(cap: usize) -> Self {
        Self {
            cap,
            stack: Vec::new(),
            started: false,
            done: false,
            bad: false,
            string: false,
            key: false,
            key_bytes: Vec::new(),
            scalar: Vec::new(),
            character: Vec::new(),
            name: None,
            capture: Vec::new(),
            capturing: false,
            oversized: false,
            values: Map::new(),
            array_key: None,
            arrays: [None; 3],
            image_usage_present: [false; 2],
            item_object: false,
            item_string: false,
            item_fields: [false; 3],
            item_key: None,
        }
    }

    pub(super) fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.bad {
                return;
            }
            self.byte(byte);
        }
    }

    fn record(&mut self, byte: u8) {
        if self.capturing && !self.oversized {
            if self.capture.len() == self.cap {
                self.oversized = true;
                if self
                    .name
                    .as_deref()
                    .is_some_and(|n| matches!(n, "status" | "error"))
                {
                    self.bad = true;
                }
                self.capture.clear();
            } else {
                self.capture.push(byte);
            }
        }
    }

    fn byte(&mut self, byte: u8) {
        if self.string {
            self.record(byte);
            self.string_byte(byte);
            return;
        }
        if !self.scalar.is_empty() {
            if matches!(byte, b',' | b'}' | b']' | b' ' | b'\n' | b'\r' | b'\t') {
                if serde_json::from_slice::<Value>(&self.scalar).is_err() {
                    self.bad = true;
                    return;
                }
                self.scalar.clear();
                self.finished_value();
            } else {
                if self.scalar.len() == 128 {
                    self.bad = true;
                    return;
                }
                self.scalar.push(byte);
                self.record(byte);
                return;
            }
        }
        if matches!(byte, b' ' | b'\n' | b'\r' | b'\t') {
            self.record(byte);
            return;
        }
        if self.done {
            self.bad = true;
            return;
        }
        if !self.started {
            self.started = true;
            if byte != b'{' {
                self.bad = true;
                return;
            }
            self.stack.push(Frame::ObjectKey(true));
            return;
        }
        let Some(frame) = self.stack.last().copied() else {
            self.bad = true;
            return;
        };
        match frame {
            Frame::ObjectKey(empty) => {
                self.record(byte);
                if byte == b'}' && empty {
                    self.close();
                } else if byte == b'"' {
                    self.string = true;
                    self.key = true;
                    self.key_bytes.clear();
                    self.key_bytes.push(byte);
                } else {
                    self.bad = true;
                }
            }
            Frame::Colon => {
                self.record(byte);
                if byte == b':' {
                    *self.stack.last_mut().unwrap() = Frame::ObjectValue;
                } else {
                    self.bad = true;
                }
            }
            Frame::ObjectEnd | Frame::ArrayEnd => {
                self.record(byte);
                match (frame, byte) {
                    (Frame::ObjectEnd, b',') => {
                        *self.stack.last_mut().unwrap() = Frame::ObjectKey(false)
                    }
                    (Frame::ArrayEnd, b',') => {
                        *self.stack.last_mut().unwrap() = Frame::ArrayValue(false)
                    }
                    (Frame::ObjectEnd, b'}') | (Frame::ArrayEnd, b']') => self.close(),
                    _ => self.bad = true,
                }
            }
            Frame::ObjectValue | Frame::ArrayValue(_) => {
                if frame == Frame::ArrayValue(true) && byte == b']' {
                    self.record(byte);
                    self.close();
                    return;
                }
                if self.stack.len() == 1 {
                    self.capturing = self.name.is_some();
                    self.oversized = false;
                    self.capture.clear();
                    if let Some(index) = self.array_key {
                        self.arrays[index] = (byte == b'[').then_some((0, true));
                    }
                } else if self.stack.len() == 2 && matches!(frame, Frame::ArrayValue(_)) {
                    self.item_object = byte == b'{';
                    self.item_string = byte == b'"';
                    self.item_fields = [false; 3];
                    self.item_key = None;
                } else if self.stack.len() == 3 && frame == Frame::ObjectValue {
                    if let Some(index) = self.item_key {
                        self.item_fields[index] = byte == b'"';
                    }
                }
                self.record(byte);
                match byte {
                    b'{' | b'[' => {
                        if self.stack.len() == 128 {
                            self.bad = true;
                            return;
                        }
                        self.stack.push(if byte == b'{' {
                            Frame::ObjectKey(true)
                        } else {
                            Frame::ArrayValue(true)
                        });
                    }
                    b'"' => {
                        self.string = true;
                        self.key = false;
                    }
                    b'-' | b'0'..=b'9' | b't' | b'f' | b'n' => self.scalar.push(byte),
                    _ => self.bad = true,
                }
            }
        }
    }

    // Validate one raw UTF-8 scalar or JSON escape at a time; even a huge discarded string has
    // a constant-size validator. Surrogate pairs remain together for serde's validation.
    fn string_byte(&mut self, byte: u8) {
        if self.key
            && (self.stack.len() == 1 || (self.stack.len() == 3 && self.array_key.is_some()))
            && self.key_bytes.len() <= 128
        {
            self.key_bytes.push(byte);
        }
        if self.character.is_empty() {
            match byte {
                b'"' => {
                    self.string = false;
                    if self.key {
                        if self.stack.len() == 1 {
                            let name = serde_json::from_slice::<String>(&self.key_bytes).ok();
                            self.array_key = match name.as_deref() {
                                Some("data") => Some(0),
                                Some("predictions") => Some(1),
                                Some("images") => Some(2),
                                _ => None,
                            };
                            if let Some(index) = self.array_key {
                                self.arrays[index] = None;
                            }
                            match name.as_deref() {
                                Some("usage") => self.image_usage_present[0] = true,
                                Some("usageMetadata") => self.image_usage_present[1] = true,
                                _ => {}
                            }
                            self.name = name.filter(|s| {
                                matches!(
                                    s.as_str(),
                                    "usage"
                                        | "usageMetadata"
                                        | "meta"
                                        | "trace"
                                        | "status"
                                        | "error"
                                        | "metrics"
                                        | "inputTextTokenCount"
                                )
                            });
                            if let Some(name) = &self.name {
                                self.values.remove(name);
                            }
                        }
                        if self.stack.len() == 3 {
                            let name = serde_json::from_slice::<String>(&self.key_bytes).ok();
                            self.item_key = match (self.array_key, name.as_deref()) {
                                (Some(0), Some("b64_json"))
                                | (Some(1), Some("bytesBase64Encoded")) => Some(0),
                                (Some(0), Some("url")) | (Some(1), Some("gcsUri")) => Some(1),
                                (Some(1), Some("raiFilteredReason")) => Some(2),
                                _ => None,
                            };
                        }
                        *self.stack.last_mut().unwrap() = Frame::Colon;
                    } else {
                        self.finished_value();
                    }
                }
                0..=31 => self.bad = true,
                b'\\' | 128..=255 => self.character.push(byte),
                _ => {}
            }
            return;
        }
        self.character.push(byte);
        if self.character[0] == b'\\' {
            let needed = if self.character.get(1) == Some(&b'u') {
                6
            } else {
                2
            };
            if self.character.len() < needed {
                return;
            }
            if needed == 6 && self.character.len() == 6 {
                let high = std::str::from_utf8(&self.character[2..])
                    .ok()
                    .and_then(|s| u16::from_str_radix(s, 16).ok())
                    .is_some_and(|n| (0xd800..=0xdbff).contains(&n));
                if high {
                    return;
                }
            }
            if self.character.len() > 6 && self.character.len() < 12 {
                return;
            }
            let mut quoted = Vec::with_capacity(14);
            quoted.push(b'"');
            quoted.extend_from_slice(&self.character);
            quoted.push(b'"');
            if serde_json::from_slice::<String>(&quoted).is_err() {
                self.bad = true;
            }
            self.character.clear();
        } else {
            match std::str::from_utf8(&self.character) {
                Ok(_) => self.character.clear(),
                Err(e) if e.error_len().is_some() || self.character.len() == 4 => self.bad = true,
                Err(_) => {}
            }
        }
    }

    fn close(&mut self) {
        self.stack.pop();
        if self.stack.is_empty() {
            self.done = true;
        } else {
            self.finished_value();
        }
    }

    fn finished_value(&mut self) {
        if self.stack.len() == 2 {
            if let Some(index) = self.array_key {
                if let Some((count, valid)) = self.arrays[index].as_mut() {
                    let payload = self.item_object && (self.item_fields[0] || self.item_fields[1]);
                    let filtered =
                        index == 1 && self.item_object && self.item_fields[2] && !payload;
                    *valid &= if index == 2 {
                        self.item_string
                    } else {
                        payload || filtered
                    };
                    if !filtered {
                        *count = count.saturating_add(1);
                    }
                }
            }
        }
        if self.stack.len() == 1 {
            self.array_key = None;
        }
        if self.stack.len() == 1 && self.capturing {
            if !self.oversized {
                match serde_json::from_slice::<Value>(&self.capture) {
                    Ok(value) => {
                        if let Some(name) = self.name.take() {
                            self.values.insert(name, value);
                        }
                    }
                    Err(_) => self.bad = true,
                }
            }
            self.capturing = false;
        }
        if let Some(frame) = self.stack.last_mut() {
            *frame = match *frame {
                Frame::ObjectValue => Frame::ObjectEnd,
                Frame::ArrayValue(_) => Frame::ArrayEnd,
                _ => {
                    self.bad = true;
                    return;
                }
            };
        }
    }

    /// Count only shape-valid provider image entries; filtered Imagen entries are not images.
    /// A reported usage member (even oversized or null) suppresses per-image cost billing.
    pub(super) fn image_count(&self, protocol: &str) -> Option<u32> {
        if self.bad || !self.done || self.string || !self.scalar.is_empty() {
            return None;
        }
        let index = match protocol {
            "openai" if !self.image_usage_present[0] => 0,
            "gemini" if !self.image_usage_present[1] => 1,
            "bedrock" => 2,
            _ => return None,
        };
        let (count, valid) = self.arrays[index]?;
        valid.then_some(count)
    }

    /// Only a completely validated document can bill through this projection. Error/status fields
    /// are retained as well, so omitting large content cannot turn a provider failure into success.
    pub(super) fn finish(&self) -> Option<Vec<u8>> {
        if self.bad || !self.done || self.string || !self.scalar.is_empty() {
            return None;
        }
        // A missing/oversized failure discriminator cannot be safely interpreted as success.
        if self.oversized
            && self
                .name
                .as_deref()
                .is_some_and(|n| matches!(n, "status" | "error"))
        {
            return None;
        }
        serde_json::to_vec(&self.values).ok()
    }
}

#[cfg(test)]
#[path = "root_usage_tests.rs"]
mod tests;
