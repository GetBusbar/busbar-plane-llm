// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The one drop path (design F3 "Drops"; spec Part 2 #76: cross-protocol no-equivalent is drop +
//! warn + covered by a test; the LLM dialect fidelity rule: translate what maps, drop what cannot).

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeSet;

use busbar_contract::diagnostic::Diagnostic;
use serde_json::{Map, Value};

/// Which way a TRANSLATE attempt runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// The caller's request, written for the far end.
    Request,
    /// The far end's answer, written for the caller.
    Response,
}

impl Direction {
    /// The word the warn carries.
    pub const fn as_str(self) -> &'static str {
        match self {
            Direction::Request => "request",
            Direction::Response => "response",
        }
    }
}

/// One TRANSLATE attempt: its direction, the caller's dialect and the far end's.
#[derive(Clone, Copy, Debug)]
pub struct Seam<'a> {
    pub direction: Direction,
    pub ingress: &'a str,
    pub egress: &'a str,
}

/// One member that did not cross: its wire path, the diagnostic its warn carries, and the warn's
/// text.
#[derive(Clone, Debug)]
pub struct Dropped {
    pub path: Cow<'static, str>,
    /// How [`note`] names the drop: by `path` as given, or by the source dialect's wire path for
    /// an IR member or a content block.
    pub at: At,
    pub diag: &'static Diagnostic,
    pub message: Cow<'static, str>,
}

impl Dropped {
    /// A drop named by its wire path.
    pub fn new(
        path: impl Into<Cow<'static, str>>,
        diag: &'static Diagnostic,
        message: impl Into<Cow<'static, str>>,
    ) -> Dropped {
        Dropped {
            path: path.into(),
            at: At::Wire,
            diag,
            message: message.into(),
        }
    }

    /// A drop of the IR slot `name`, named on the open attempt by the caller's wire path for it
    /// (by `name` itself when the caller's map file has no row for it).
    pub fn slot(
        name: &'static str,
        diag: &'static Diagnostic,
        message: impl Into<Cow<'static, str>>,
    ) -> Dropped {
        Dropped {
            path: Cow::Borrowed(name),
            at: At::Slot(&[]),
            diag,
            message: message.into(),
        }
    }

    /// The IR names whose rows spell this slot drop's wire path, first found wins (default: the
    /// slot's own name).
    pub fn rows(mut self, rows: &'static [&'static str]) -> Dropped {
        self.at = At::Slot(rows);
        self
    }

    /// A writer's drop of `member` ([`writer_drop!`]).
    pub fn member(
        member: Member,
        diag: &'static Diagnostic,
        message: impl Into<Cow<'static, str>>,
    ) -> Dropped {
        match member {
            Member::Slot(name, rows) => Dropped::slot(name, diag, message).rows(rows),
            Member::Block(kind) => Dropped {
                path: Cow::Borrowed(kind),
                at: At::Block,
                diag,
                message: message.into(),
            },
            Member::Wire(path) => Dropped::new(path, diag, message),
        }
    }
}

/// How a [`Dropped`] is named.
#[derive(Clone, Copy, Debug)]
pub enum At {
    /// `path` is the wire path.
    Wire,
    /// `path` is an IR member's name: on a request, the caller's wire path for it ([`wire_path`],
    /// read from the listed IR names' rows, or `path`'s own when the list is empty).
    Slot(&'static [&'static str]),
    /// `path` is an IR block kind. The current reader-carried block origin names the native
    /// container; absent metadata is explicitly reported as an IR-only origin.
    Block,
}

/// What a writer drops ([`writer_drop!`]): an IR member, or a content block of an IR kind.
#[derive(Clone, Copy, Debug)]
pub enum Member {
    /// The IR member `name`, named by the first of the listed IR names the source dialect has a row
    /// for (empty: `name` itself).
    Slot(&'static str, &'static [&'static str]),
    /// A content block of the IR kind (`image`, `document`, `audio`, `video`, `thinking`, `text`,
    /// `tool_use`, `tool_result`), named by the source dialect's block container.
    Block(&'static str),
    /// A member of the SOURCE dialect's own wire at this path (notation A): what a READER drops of
    /// the bytes it reads (the caller's on a request, the far end's on an answer) because the IR has
    /// no place for it.
    Wire(&'static str),
}

/// The IR member `name`, by its own rows.
pub const fn member(name: &'static str) -> Member {
    Member::Slot(name, &[])
}

/// The reasoning ask: named by the caller's effort row, else its budget row.
pub const REASONING: Member = Member::Slot(
    "reasoning",
    &["reasoning_effort", "reasoning", "thinking_budget"],
);

/// A content block of the IR kind `kind`.
pub const fn block(kind: &'static str) -> Member {
    Member::Block(kind)
}

/// A member of the reading dialect's own wire at `path` (a reader's drop).
pub const fn wire(path: &'static str) -> Member {
    Member::Wire(path)
}

/// The IR's content-block kinds ([`crate::codec::ir::IrBlock::kind_name`]), spelled once: the
/// dialects' `IR_BLOCK_KINDS` tables and their writers' block drops name them from here.
pub mod kind {
    pub const TEXT: &str = "text";
    pub const IMAGE: &str = "image";
    pub const DOCUMENT: &str = "document";
    pub const AUDIO: &str = "audio";
    pub const VIDEO: &str = "video";
    pub const THINKING: &str = "thinking";
    pub const TOOL_USE: &str = "tool_use";
    pub const TOOL_RESULT: &str = "tool_result";
    pub const HOSTED_TOOL: &str = "hosted_tool";
}

/// A text block.
pub const TEXT: Member = block(kind::TEXT);
/// An image block.
pub const IMAGE: Member = block(kind::IMAGE);
/// A document block.
pub const DOCUMENT: Member = block(kind::DOCUMENT);
/// An audio block.
pub const AUDIO: Member = block(kind::AUDIO);
/// A video block.
pub const VIDEO: Member = block(kind::VIDEO);
/// A thinking (reasoning) block; the reasoning ASK is [`REASONING`].
pub const THINKING: Member = block(kind::THINKING);
/// A provider-run tool's record.
pub const HOSTED_TOOL: Member = block(kind::HOSTED_TOOL);

/// The IR request members a drop names, spelled once (the dialects' `REQUEST_CODE_NAMES` and
/// `UNREAD` tables name them from here).
pub mod name {
    pub const N: &str = "n";
    pub const REASONING: &str = "reasoning";
    pub const THINKING_BUDGET: &str = "thinking_budget";
    pub const CACHE_CONTROL: &str = "cache_control";
    pub const STOP: &str = "stop";
    pub const TOOLS: &str = "tools";
    pub const TOOL_CHOICE: &str = "tool_choice";
    pub const PARALLEL_TOOL_CALLS: &str = "parallel_tool_calls";
    pub const RESPONSE_FORMAT: &str = "response_format";
    pub const METADATA: &str = "metadata";
    pub const OUTPUT_MODALITIES: &str = "output_modalities";
    pub const TOP_LOGPROBS: &str = "top_logprobs";
    pub const TOP_K: &str = "top_k";
    pub const SERVICE_TIER: &str = "service_tier";
    pub const LOGPROBS: &str = "logprobs";
    pub const STRICT: &str = "strict";
}

/// The IR members a writer drops by name.
pub const TOOLS: Member = member(name::TOOLS);
pub const TOOL_CHOICE: Member = member(name::TOOL_CHOICE);
pub const PARALLEL_TOOL_CALLS: Member = member(name::PARALLEL_TOOL_CALLS);
pub const RESPONSE_FORMAT: Member = member(name::RESPONSE_FORMAT);
pub const METADATA: Member = member(name::METADATA);
pub const OUTPUT_MODALITIES: Member = member(name::OUTPUT_MODALITIES);
pub const TOP_LOGPROBS: Member = member(name::TOP_LOGPROBS);
pub const SERVICE_TIER: Member = member(name::SERVICE_TIER);
pub const LOGPROBS: Member = member(name::LOGPROBS);
pub const STRICT: Member = member(name::STRICT);

/// Whether a TRANSLATE attempt is open on this thread.
pub fn is_open() -> bool {
    OPEN.with(|o| o.borrow().is_some())
}

/// The writer drop (and a reader's, via [`wire`]): a member or block that does not cross. Inside a
/// translate attempt it is [`note`]d (one warn per path naming the source dialect's wire path, plus
/// the audit); outside one (same-dialect write, direct writer call, relay tap) the writer's own warn
/// is emitted unchanged and nothing is audited. `fields` are the writer warn's own fields (each
/// followed by a comma).
/// `writer_drop!(member("tool_choice"), &DIAG, [count = n,], "dropping ... {x}", x = 1)`
macro_rules! writer_drop {
    ($member:expr, $diag:expr, [$($field:tt)*], $($msg:tt)+) => {{
        if $crate::codec::drops::is_open() {
            $crate::codec::drops::note($crate::codec::drops::Dropped::member(
                $member,
                $diag,
                format!($($msg)+),
            ));
        } else {
            tracing::warn!($($field)* $($msg)+);
        }
    }};
}
pub(crate) use writer_drop;

/// The name resolver: the wire path (notation A, as its map file's rows spell it) at which request
/// dialect `dialect` carries the IR name `name` ([`crate::codec::carry::wire_path`]; the first of
/// `rows` it has a row for, when any), else `name`.
pub fn wire_path(dialect: &str, name: &str, rows: &[&str]) -> String {
    resolve(dialect, name, rows).unwrap_or_else(|| name.to_string())
}

/// [`wire_path`]: a map-file row for it, else the path the dialect's reader carries it from by code
/// ([`crate::codec::proto_codec::ProtocolReader::request_code_names`]); `None` when it has neither
/// (the drop is then named by the IR name, which the drop-names census holds at 0).
pub fn resolve(dialect: &str, name: &str, rows: &[&str]) -> Option<String> {
    let rows = if rows.is_empty() { &[name][..] } else { rows };
    crate::codec::proto_codec::with_reader(dialect, |r| {
        rows.iter().find_map(|n| {
            crate::codec::carry::wire_path(r.grammar().request_map, n).or_else(|| {
                r.grammar()
                    .request_code_names
                    .iter()
                    .find(|(ir, _)| ir == n)
                    .map(|(_, path)| path.to_string())
            })
        })
    })
    .flatten()
}

/// The CALLER's wire path for the IR name `name`, for a drop warned outside [`note`] (a writer's
/// control warn): on an open request attempt, [`wire_path`] in the caller's dialect; else `name`.
pub fn caller_path(name: &str) -> String {
    let ingress = OPEN.with(|o| {
        o.borrow()
            .as_ref()
            .filter(|o| o.direction == Direction::Request)
            .map(|o| o.ingress.clone())
    });
    match ingress {
        Some(ingress) => wire_path(&ingress, name, &[]),
        None => name.to_string(),
    }
}

/// Diagnostic detail is a bounded prefix; the final entry records omitted detail.
const MAX_DROP_PATHS: usize = 256;
const MAX_DROP_BYTES: usize = 64 * 1024;
const MAX_DROP_PATH_BYTES: usize = 4096;
const OMITTED_PATH: &str = "[additional dropped paths omitted]";

#[derive(Default)]
struct Paths {
    ordered: Vec<String>,
    index: BTreeSet<String>,
    bytes: usize,
    full: bool,
}

impl Paths {
    fn omit(&mut self) {
        if !self.full {
            self.full = true;
            self.bytes += OMITTED_PATH.len();
            self.index.insert(OMITTED_PATH.to_string());
            self.ordered.push(OMITTED_PATH.to_string());
        }
    }

    fn push(&mut self, path: &str) -> bool {
        if self.full || self.index.contains(path) {
            return false;
        }
        if path == OMITTED_PATH
            || self.ordered.len() >= MAX_DROP_PATHS - 1
            || path.len() > MAX_DROP_PATH_BYTES
            || path.len() > MAX_DROP_BYTES - OMITTED_PATH.len() - self.bytes
        {
            self.omit();
            return false;
        }
        self.bytes += path.len();
        self.index.insert(path.to_string());
        self.ordered.push(path.to_string());
        true
    }

    fn push_parts(&mut self, parts: &[&str]) {
        if self.full {
            return;
        }
        if parts
            .iter()
            .try_fold(0usize, |n, p| n.checked_add(p.len()))
            .is_none_or(|n| n > MAX_DROP_PATH_BYTES)
        {
            self.omit();
            return;
        }
        self.push(&parts.concat());
    }
}

/// The open attempt on this thread.
struct Open {
    direction: Direction,
    ingress: String,
    egress: String,
    paths: Paths,
    block_origin: Option<crate::codec::ir::IrBlockOrigin>,
}

/// The diagnostic origin for one synchronous writer projection. It cannot cross threads.
pub(crate) struct BlockScope {
    outer: Option<Option<crate::codec::ir::IrBlockOrigin>>,
    not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Drop for BlockScope {
    fn drop(&mut self) {
        if let Some(outer) = self.outer.take() {
            OPEN.with(|slot| {
                if let Some(open) = slot.borrow_mut().as_mut() {
                    open.block_origin = outer;
                }
            });
        }
    }
}

pub(crate) fn block_scope(origin: Option<&crate::codec::ir::IrBlockOrigin>) -> BlockScope {
    BlockScope {
        outer: OPEN.with(|slot| {
            slot.borrow_mut()
                .as_mut()
                .map(|open| std::mem::replace(&mut open.block_origin, origin.cloned()))
        }),
        not_send: std::marker::PhantomData,
    }
}

/// Run a writer's projection of one block with its captured origin. Scope exit restores the
/// enclosing block even on early return or unwind.
pub fn with_block_origin<T>(
    origin: Option<&crate::codec::ir::IrBlockOrigin>,
    f: impl FnOnce() -> T,
) -> T {
    let _scope = block_scope(origin);
    f()
}

/// Project the exact child entry retained by the reader, without deriving a native path from
/// the child's kind or from the enclosing target protocol.
pub(crate) fn child_scope(index: usize) -> BlockScope {
    let origin = OPEN.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|open| open.block_origin.as_ref()?.children.get(index).cloned())
    });
    block_scope(origin.as_ref())
}

pub fn with_child_origin<T>(index: usize, f: impl FnOnce() -> T) -> T {
    let _scope = child_scope(index);
    f()
}

thread_local! {
    /// The open translate attempt: a dynamic scope for one synchronous call. Set only by a
    /// [`ScopeGuard`] and held only while it lives; a guard never spans an `await`, a yield, a
    /// ticket boundary or a return to the driver (every [`scope`] / [`scope_more`] runs its closure to
    /// completion on the calling thread), so nothing written here outlives the call or crosses units.
    static OPEN: RefCell<Option<Open>> = const { RefCell::new(None) };
}

/// One open translate attempt. Opening installs it on this thread and keeps the enclosing one
/// (scopes nest); closing it ([`ScopeGuard::close`], or `Drop` on early return or unwind) restores
/// the enclosing one, so a scope is never left open and its log never leaks into the next call.
pub(crate) struct ScopeGuard {
    outer: Option<Option<Open>>,
    thread: std::thread::ThreadId,
}

impl ScopeGuard {
    /// Open an attempt for `seam`, with the paths already dropped (a stream's earlier frames; empty if fresh).
    pub(crate) fn open(seam: Seam<'_>, paths: Vec<String>) -> ScopeGuard {
        let mut bounded = Paths::default();
        for path in paths {
            if bounded.index.contains(&path) && !bounded.full {
                if bounded.ordered.len() >= MAX_DROP_PATHS - 1
                    || path.len() > MAX_DROP_BYTES - OMITTED_PATH.len() - bounded.bytes
                {
                    bounded.omit();
                } else {
                    bounded.bytes += path.len();
                    bounded.ordered.push(path);
                }
            } else {
                bounded.push(&path);
            }
            if bounded.full {
                break;
            }
        }
        let open = Open {
            direction: seam.direction,
            ingress: seam.ingress.to_string(),
            egress: seam.egress.to_string(),
            paths: bounded,
            block_origin: None,
        };
        ScopeGuard {
            outer: Some(OPEN.with(|o| o.replace(Some(open)))),
            thread: std::thread::current().id(),
        }
    }

    /// Close the attempt, putting the enclosing one back, and hand back the paths it dropped.
    pub(crate) fn close(mut self) -> Vec<String> {
        self.check_thread();
        let outer = self.outer.take().unwrap_or(None);
        OPEN.with(|o| o.replace(outer))
            .map(|o| o.paths.ordered)
            .unwrap_or_default()
    }

    /// A guard is closed on the thread that opened it; a debug build panics otherwise (not while
    /// unwinding, where a second panic would abort).
    fn check_thread(&self) {
        if cfg!(debug_assertions) && !std::thread::panicking() {
            assert_eq!(
                std::thread::current().id(),
                self.thread,
                "a drop scope was closed on a thread other than the one that opened it: a scope \
                 never spans an await, a ticket boundary or a return to the driver"
            );
        }
    }
}

impl std::ops::Drop for ScopeGuard {
    fn drop(&mut self) {
        if let Some(outer) = self.outer.take() {
            self.check_thread();
            OPEN.with(|o| o.replace(outer));
        }
    }
}

/// Run `f` as one translate attempt: every [`note`] inside warns once per path, and the paths come
/// back in first-dropped order for the seam's audit. Diagnostics retain at most 255
/// concrete entries plus an omission marker, 64 KiB total and 4 KiB per path.
/// Once any limit is exceeded, further detail and warnings are suppressed.
pub fn scope<T>(seam: Seam<'_>, f: impl FnOnce() -> T) -> (T, Vec<String>) {
    let guard = ScopeGuard::open(seam, Vec::new());
    let out = f();
    let paths = guard.close();
    (out, paths)
}

/// A caller-authored IR block may have no native source metadata. Keep that absence explicit;
/// a grammar or normalized variant cannot recover the native container that supplied it.
const UNATTRIBUTED_BLOCK_PATH: &str = "ir.block.kind";
const UNATTRIBUTED_BLOCK_DETAIL: &str = "native block source metadata is unavailable";

/// Drop one member on the open attempt: one warn naming its captured native path (first time
/// that path drops), retained for the audit.
pub fn note(d: Dropped) {
    let context = OPEN.with(|slot| {
        slot.borrow().as_ref().map(|open| {
            (
                open.direction,
                open.ingress.clone(),
                open.egress.clone(),
                open.block_origin.as_ref().and_then(|origin| origin.source),
            )
        })
    });
    let Some((direction, ingress, egress, block_origin)) = context else {
        return;
    };
    let source = match direction {
        Direction::Request => ingress.as_str(),
        Direction::Response => egress.as_str(),
    };
    let d = match d.at {
        At::Slot(rows) if direction == Direction::Request => Dropped {
            path: Cow::Owned(wire_path(source, &d.path, rows)),
            ..d
        },
        At::Block => match block_origin {
            Some(origin) => Dropped {
                path: Cow::Borrowed(origin.path),
                ..d
            },
            None => Dropped {
                path: Cow::Owned(format!("{UNATTRIBUTED_BLOCK_PATH}={}", d.path)),
                message: Cow::Owned(format!("{}; {UNATTRIBUTED_BLOCK_DETAIL}", d.message)),
                ..d
            },
        },
        _ => d,
    };
    let added = OPEN.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(open) = slot.as_mut() else {
            return false;
        };
        open.paths.push(&d.path)
    });
    if !added {
        return;
    }
    busbar_contract::diag_warn!(
        d.diag, direction = direction.as_str(), ingress = %ingress, egress = %egress,
        path = %d.path, "{}", d.message
    );
}

/// What a member a reader parks in `extra` holds of the caller's request, beside the members it
/// parks because its map file does not model them (which are named by their own key).
#[derive(Clone, Copy, Debug)]
pub enum Holds {
    /// Nothing that is dropped: a spelling hint, a member the dialect carries by its own code, or a
    /// stash whose content another declaration names (the block grammar).
    Nothing,
    /// The caller's member at this wire path.
    Path(&'static str),
    /// An object only partly carried by the dialect's code: each member is named `<key>.<member>`,
    /// except the listed ones and those the map file maps.
    Members(&'static [&'static str]),
    /// An object of per-item objects (keyed by item index): each item member is named
    /// `<path>.<member>`, except the listed ones.
    Items(&'static str, &'static [&'static str]),
}

/// A member a dialect's reader parks in `extra` and what it holds of the caller's request.
#[derive(Clone, Copy, Debug)]
pub struct Parked {
    pub key: &'static str,
    pub holds: Holds,
}

/// The wire paths of the request members `extra` holds, none of which a TRANSLATE attempt carries:
pub fn extra_paths(
    extra: &Map<String, Value>,
    parked: &[Parked],
    mapped: impl Fn(&[&str]) -> bool,
) -> Vec<String> {
    let mut out = Paths::default();
    for (key, value) in extra {
        if out.full {
            break;
        }
        match parked.iter().find(|p| p.key == key).map(|p| p.holds) {
            None => {
                out.push(key);
            }
            Some(Holds::Nothing) => {}
            Some(Holds::Path(path)) => {
                out.push(path);
            }
            Some(Holds::Members(except)) => {
                for member in value.as_object().into_iter().flat_map(Map::keys) {
                    if out.full {
                        break;
                    }
                    if !except.contains(&member.as_str())
                        && !mapped(&[key.as_str(), member.as_str()])
                    {
                        out.push_parts(&[key, ".", member]);
                    }
                }
            }
            Some(Holds::Items(path, except)) => {
                let items = value.as_object().into_iter().flat_map(Map::values);
                for member in items.flat_map(|i| i.as_object().into_iter().flat_map(Map::keys)) {
                    if out.full {
                        break;
                    }
                    if !except.contains(&member.as_str()) {
                        out.push_parts(&[path, ".", member]);
                    }
                }
            }
        }
    }
    out.ordered
}

/// A dialect's content-block grammar, as data: where its blocks are, how a block names its kind,
/// and the kinds its reader models. A block of any other kind does not cross a TRANSLATE attempt.
#[derive(Clone, Copy, Debug)]
pub struct Blocks {
    /// Where the blocks are, in notation-A steps: `["messages[]", "content[]"]`.
    pub at: &'static [&'static str],
    /// The member naming a block's kind (`{"type": "text", ...}`); `None` for a union keyed by its
    /// one kind member (`{"text": ...}`).
    pub tag: Option<&'static str>,
    /// The kinds the reader models.
    pub modelled: &'static [&'static str],
    /// Members a keyed block carries beside its kind (`thoughtSignature`); never a kind.
    pub companions: &'static [&'static str],
}

impl Blocks {
    /// The kind of `block` the reader does not model (`""` for a tagged block that names no kind);
    /// `None` when it models it, or when `block` is not an object at all (wrong-typed input the
    /// reader refuses in its own error envelope).
    fn unmodelled_kind<'v>(&self, block: &'v Value) -> Option<&'v str> {
        let obj = block.as_object()?;
        match self.tag {
            Some(tag) => match obj.get(tag).and_then(Value::as_str) {
                Some(kind) if self.modelled.contains(&kind) => None,
                Some(kind) => Some(kind),
                None => Some(""),
            },
            None => {
                if obj.keys().any(|k| self.modelled.contains(&k.as_str())) {
                    return None;
                }
                obj.keys()
                    .map(String::as_str)
                    .find(|k| !self.companions.contains(k))
            }
        }
    }

    /// Whether the reader models `block`.
    pub fn models(&self, block: &Value) -> bool {
        self.unmodelled_kind(block).is_none()
    }

    /// The notation-A path of every block kind in `body` the reader does not model, each once, in
    /// the order met: `messages[].content[].type=document`, or `contents[].parts[].toolCall` for a
    /// keyed block.
    pub fn unmodelled(&self, body: &Value) -> Vec<String> {
        let at = self.at.join(".");
        let mut out = Paths::default();
        visit(body, self.at, &mut |block| {
            if let Some(kind) = self.unmodelled_kind(block) {
                match self.tag {
                    Some(_) if kind.is_empty() => {
                        out.push(&at);
                    }
                    Some(tag) => out.push_parts(&[&at, ".", tag, "=", kind]),
                    None => out.push_parts(&[&at, ".", kind]),
                }
            }
            !out.full
        });
        out.ordered
    }
}

/// Visit matching blocks without retaining an input-sized intermediate collection.
fn visit(v: &Value, at: &[&str], f: &mut impl FnMut(&Value) -> bool) -> bool {
    let Some((step, rest)) = at.split_first() else {
        return f(v);
    };
    match step.strip_suffix("[]") {
        Some(key) => {
            for item in v.get(key).and_then(Value::as_array).into_iter().flatten() {
                if !visit(item, rest, f) {
                    return false;
                }
            }
        }
        None => {
            if let Some(inner) = v.get(*step) {
                return visit(inner, rest, f);
            }
        }
    }
    true
}

/// The warn of a request block the far end's dialect has no form for.
pub const UNMODELLED_REQUEST_BLOCK: &str =
    "dropping a request content block on the cross-protocol seam: the source dialect's block \
     kind has no form in the target dialect, so it is NOT forwarded to the backend (nothing is \
     put in its place). Route the request to a same-protocol lane if the block is load-bearing";

/// The warn of an answer block the caller's dialect has no form for.
pub const UNMODELLED_ANSWER_BLOCK: &str =
    "dropping an answer content block on the cross-protocol seam: the upstream dialect's block \
     kind has no form in the caller's dialect, so it is NOT delivered (nothing is put in its \
     place). Route the request to a same-protocol lane if the block is load-bearing";

/// Drop, on the open attempt, every block of `body` that `grammar` does not model, with `message`.
pub fn note_unmodelled_blocks(grammar: &[Blocks], body: &Value, message: &'static str) {
    for path in grammar.iter().flat_map(|g| g.unmodelled(body)) {
        note(Dropped::new(
            path,
            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
            message,
        ));
    }
}

/// Run `f` as one more piece of a translate attempt spanning calls (a stream, one frame per call): a
/// path in `seen` was already warned and is not warned again; a newly dropped path is warned and
/// appended to `seen` for the audit at the attempt's end, within [`scope`]'s limits.
/// The omission marker persists across frames, so saturation cannot reset.
pub fn scope_more<T>(seam: Seam<'_>, seen: &mut Vec<String>, f: impl FnOnce() -> T) -> T {
    let guard = ScopeGuard::open(seam, std::mem::take(seen));
    let out = f();
    *seen = guard.close();
    out
}

/// What a dialect's answers carry in one direction:
/// the paths its map file lists (`map`, generated) and those its code carries that no row names
/// (`code`: a structural member, keepalive, terminal error). Any other answer member does not cross
/// a translate attempt, and the walk names it.
#[derive(Clone, Copy, Debug)]
pub struct Carried {
    pub map: &'static [&'static str],
    pub code: &'static [&'static str],
    /// Paths inside a carried subtree that the code does not carry (a member of a modelled block, a
    /// vendor detail under a member it reads), named like any drop. Sorted ascending: the walk asks
    /// it of every member of every stream frame, so it is searched, not scanned (test
    /// `every_drop_list_is_sorted_for_the_walks_search`). A path under a `reroot` prefix is listed re-rooted.
    pub drops: &'static [&'static str],
    /// Stream frames whose member restates an answer's (a Responses `output_item` frame's `item`
    /// is an `output[]` element): such paths are read in the answer's list, which is stated once.
    pub reroot: Option<Reroot>,
    /// Request defaults the far end echoes in its answer, each a member's key and its default as
    /// JSON text (`("tool_choice", "\"auto\"")`). A member equal to its default carries nothing the
    /// caller set, so the walk does not name it; any other value is named like every drop.
    pub defaults: &'static [(&'static str, &'static str)],
}

/// Where a stream's paths re-root onto its answer's: each `(from, to)` spells a path at or under
/// `from` as the same path under `to`, looked up in the stream's `drops` and in `answer_drops`.
/// The walk names the path as the wire spells it (the warn and the audit row are unchanged).
#[derive(Clone, Copy, Debug)]
pub struct Reroot {
    pub at: &'static [(&'static str, &'static str)],
    pub answer_drops: &'static [&'static str],
}

/// What a translate attempt walks for one dialect, as data: its request field table (map file), the
/// members its reader parks in `extra`, its request and answer content-block grammars, what its
/// buffered answers and stream frames carry (`None` walks nothing), whether stream frames are keyed
/// by event name (the event roots their paths: Bedrock's `contentBlockDelta.delta...`), how it
/// spells each IR block kind for a dropped block's warn, and the request members its reader carries
/// by code from a path no map row names (`("response_format", "output_config.format")`).
#[derive(Clone, Copy)]
pub struct Grammar {
    pub request_map: crate::codec::carry::Table,
    pub parked: &'static [Parked],
    pub request_blocks: &'static [Blocks],
    pub response_blocks: &'static [Blocks],
    pub response_carried: Option<Carried>,
    pub stream_carried: Option<Carried>,
    pub stream_keyed_by_event: bool,
    pub block_kinds: &'static [(&'static str, &'static str)],
    pub request_code_names: &'static [(&'static str, &'static str)],
}

impl Grammar {
    /// A dialect that declares nothing: every walk is empty.
    pub const NONE: Self = Self {
        request_map: &[],
        parked: &[],
        request_blocks: &[],
        response_blocks: &[],
        response_carried: None,
        stream_carried: None,
        stream_keyed_by_event: false,
        block_kinds: &[],
        request_code_names: &[],
    };
}

/// Where a wire path stands against what a dialect carries.
enum Cover {
    /// It, or a path above it, is carried: its whole subtree crosses.
    Carried,
    /// A carried path lies below it: walk into it.
    Above,
    /// Nothing at or below it is carried: it is dropped.
    Unmapped,
}

/// `path` lies under `top` (`top.x`, `top[]...`, `top.type=x`).
fn under(path: &str, top: &str) -> bool {
    path.len() > top.len()
        && path.starts_with(top)
        && matches!(path.as_bytes()[top.len()], b'.' | b'[')
}

fn join(path: &str, step: &str) -> String {
    if path.is_empty() {
        step.to_string()
    } else {
        format!("{path}.{step}")
    }
}

/// The run of the sorted `table` that starts with `prefix` (one contiguous run in a sorted table).
fn prefixed<'t>(
    table: &'t [&'static str],
    prefix: &'t str,
) -> impl Iterator<Item = &'t &'static str> {
    let start = table.partition_point(|q| *q < prefix);
    table[start..]
        .iter()
        .take_while(move |q| q.starts_with(prefix))
}

impl Carried {
    /// The carried rows: the map file's and the code's (a few dozen; scanned).
    fn rows(&self) -> impl Iterator<Item = &&'static str> {
        self.map.iter().chain(self.code.iter())
    }

    /// `path` as the drop lists spell it, and the answer's list it is read in too when it lies
    /// under a `reroot` prefix.
    fn spelled<'p>(&self, path: &'p str) -> (Cow<'p, str>, &'static [&'static str]) {
        let Some(r) = self.reroot else {
            return (Cow::Borrowed(path), &[]);
        };
        match r
            .at
            .iter()
            .find(|(from, _)| path == *from || under(path, from))
        {
            Some((from, to)) => (
                Cow::Owned(format!("{to}{}", &path[from.len()..])),
                r.answer_drops,
            ),
            None => (Cow::Borrowed(path), &[]),
        }
    }

    /// The listed drops that start with `prefix` (already spelled): one contiguous run of each
    /// sorted list.
    fn listed<'a>(
        &'a self,
        prefix: &'a str,
        answer: &'static [&'static str],
    ) -> impl Iterator<Item = &'a &'static str> {
        prefixed(self.drops, prefix).chain(prefixed(answer, prefix))
    }

    /// A `reroot` prefix lies under `path`: drops are held below it.
    fn reroots_below(&self, path: &str) -> bool {
        self.reroot
            .is_some_and(|r| r.at.iter().any(|(from, _)| under(from, path)))
    }

    fn cover(&self, path: &str) -> Cover {
        let (spelled, answer) = self.spelled(path);
        let spelled = &*spelled;
        let exact = |list: &[&str]| list.binary_search_by(|d| (*d).cmp(spelled)).is_ok();
        if exact(self.drops) || exact(answer) {
            return Cover::Unmapped;
        }
        let holds_drop =
            self.reroots_below(path) || self.listed(spelled, answer).any(|d| under(d, spelled));
        let carried = self.rows().any(|q| path == *q || under(path, q));
        if carried {
            return if holds_drop {
                Cover::Above
            } else {
                Cover::Carried
            };
        }
        if holds_drop || self.rows().any(|q| under(q, path)) {
            Cover::Above
        } else {
            Cover::Unmapped
        }
    }

    /// `value`, under `key`, is the default the dialect declares the far end echoes for it.
    fn echoes_default(&self, key: &str, value: &Value) -> bool {
        self.defaults.iter().any(|(k, default)| {
            *k == key && serde_json::from_str::<Value>(default).is_ok_and(|d| d == *value)
        })
    }

    /// The objects at `path` are a union keyed by their `type` member (notation A `type=<arm>`).
    fn uses_arms(&self, path: &str) -> bool {
        let arm = join(path, "type=");
        if self.rows().any(|q| q.starts_with(&arm))
            || self
                .reroot
                .is_some_and(|r| r.at.iter().any(|(from, _)| from.starts_with(&arm)))
        {
            return true;
        }
        let (spelled, answer) = self.spelled(&arm);
        let listed = self.listed(&spelled, answer).next().is_some();
        listed
    }

    /// Whether the walk names the member at `path` (or the member above it that holds it) when an
    /// answer carries it: the walk's own decision, asked of one wire path.
    pub fn names_path(&self, path: &str) -> bool {
        let cuts = path
            .char_indices()
            .filter(|(_, c)| matches!(c, '.' | '['))
            .map(|(i, _)| i)
            .chain(std::iter::once(path.len()));
        for end in cuts {
            match self.cover(&path[..end]) {
                Cover::Unmapped => return true,
                Cover::Carried => return false,
                Cover::Above => {}
            }
        }
        false
    }

    /// The notation-A path of every member of `body` (read at `root`, `""` for the document's
    /// root) that this dialect does not carry, the shallowest only, each once, in the order met.
    pub fn unmapped(&self, root: &str, body: &Value) -> Vec<String> {
        let mut out = Paths::default();
        if root.len() > MAX_DROP_PATH_BYTES {
            out.omit();
        } else if !root.is_empty() {
            match self.cover(root) {
                Cover::Carried => return Vec::new(),
                Cover::Unmapped => {
                    out.push(root);
                    return out.ordered;
                }
                Cover::Above => {}
            }
        }
        self.walk(body, root, &mut out);
        out.ordered
    }

    fn walk(&self, v: &Value, path: &str, out: &mut Paths) {
        if out.full {
            return;
        }
        match v {
            Value::Array(items) => {
                if path.len() + 2 > MAX_DROP_PATH_BYTES {
                    out.omit();
                    return;
                }
                let p = format!("{path}[]");
                for item in items {
                    if out.full {
                        break;
                    }
                    self.walk(item, &p, out);
                }
            }
            Value::Object(obj) => {
                let arm = obj
                    .get(crate::codec::keys::TYPE)
                    .and_then(Value::as_str)
                    .filter(|_| self.uses_arms(path));
                let base = match arm {
                    Some(kind) => {
                        if path.len().saturating_add(kind.len()).saturating_add(6)
                            > MAX_DROP_PATH_BYTES
                        {
                            out.omit();
                            return;
                        }
                        let b = join(path, &format!("type={kind}"));
                        match self.cover(&b) {
                            Cover::Carried => return,
                            Cover::Unmapped => {
                                out.push(&b);
                                return;
                            }
                            Cover::Above => b,
                        }
                    }
                    None => path.to_string(),
                };
                for (key, child) in obj {
                    if out.full {
                        break;
                    }
                    if child.is_null()
                        || (arm.is_some() && key == crate::codec::keys::TYPE)
                        || self.echoes_default(key, child)
                    {
                        continue;
                    }
                    if base.len().saturating_add(key.len()).saturating_add(1) > MAX_DROP_PATH_BYTES
                    {
                        out.omit();
                        break;
                    }
                    let p = join(&base, key);
                    match self.cover(&p) {
                        Cover::Carried => {}
                        Cover::Unmapped => {
                            out.push(&p);
                        }
                        Cover::Above => self.walk(child, &p, out),
                    }
                }
            }
            _ => {}
        }
    }
}

/// The warn of an answer member the caller's dialect has no form for.
pub const UNMAPPED_ANSWER_MEMBER: &str =
    "dropping an answer member on the cross-protocol seam: the far end's dialect carries it and \
     the caller's has no form for it, so it is NOT delivered (nothing is put in its place). Route \
     the request to a same-protocol lane if the member is load-bearing";

/// Drop, on the open attempt, every member of `body` (read at `root`) that `carried` does not
/// carry.
pub fn note_unmapped(carried: &Carried, root: &str, body: &Value) {
    for path in carried.unmapped(root, body) {
        note(Dropped::new(
            path,
            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
            UNMAPPED_ANSWER_MEMBER,
        ));
    }
}

/// Read one far-end stream frame on a translate attempt, for either host (the engine's stream
/// translator, the plane's own crossing; design F7): the far end's `reader` decodes it and the drop
/// walk names what the caller's dialect will not get, once per path per stream (paths already
/// dropped ride `state.dropped`, read for the audit at stream end).
pub fn read_stream_frame(
    host: &dyn crate::codec::cells::CodecHost,
    seam: Seam<'_>,
    reader: &dyn crate::codec::proto_codec::ProtocolReader,
    event_type: &str,
    data: &Value,
    state: &mut crate::codec::ir::StreamDecodeState,
) -> Vec<crate::codec::ir::IrStreamEvent> {
    if seam.ingress == seam.egress {
        return reader.read_response_events(host, event_type, data, state);
    }
    let mut seen = std::mem::take(&mut state.dropped);
    let events = scope_more(seam, &mut seen, || {
        let events = reader.read_response_events(host, event_type, data, state);
        if let Some(carried) = reader.grammar().stream_carried {
            let root = if reader.grammar().stream_keyed_by_event {
                event_type
            } else {
                ""
            };
            note_unmapped(&carried, root, data);
        }
        events
    });
    state.dropped = seen;
    events
}

#[cfg(test)]
#[path = "tests/drops_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/drop_names_tests.rs"]
mod names_tests;

#[cfg(test)]
#[path = "tests/drop_writer_tests.rs"]
mod writer_tests;

#[cfg(test)]
#[path = "tests/drop_census_tests.rs"]
mod census_tests;

#[cfg(test)]
#[path = "tests/drop_index_tests.rs"]
mod index_tests;

#[cfg(test)]
#[path = "tests/drop_bounds_tests.rs"]
mod bounds_tests;
