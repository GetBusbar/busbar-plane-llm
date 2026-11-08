// SPDX-License-Identifier: Apache-2.0
//! Actual LLM door operations and the concrete cases this plugin declares.
use busbar_contract::abi::mechanism::call::{InHead, Outcome, BLOB_JSON};
use busbar_contract::abi::mechanism::lifecycle::{ReleaseIn, ValidateIn};
use busbar_plugin_loader::dispatch::{
    in_head, load_dropped, load_linked, out_head, rendering_of, Bind, ConnTable, DispatchConfig,
    Done, InFrame, LinkedRow, NoSink, OutFrame,
};
use std::sync::Arc;
use std::time::Duration;

use busbar_contract::abi::hook::SignalEntry;
use busbar_contract::abi::mechanism::call::{
    AbiStr, Blob, DeadlineClass, Field, OutHead, Span, BLOB_OCTETS,
};
use busbar_contract::abi::mechanism::lifecycle::{slot as life, GenIn, RefreshIn};
use busbar_contract::abi::mechanism::ticket::Ticket;
use busbar_contract::abi::plane::{
    slot, ArriveIn, ArriveOut, OnPieceIn, OnPieceOut, OutField, PlaneOpenIn, PlaneOpenOut,
    PlaneRefreshOut, ProjectIn, ProjectOut, RecordWrite, RefusalIn, RefusalOut, ServeIn, ServeOut,
    UnitCount, EMIT_DONE, EMIT_TO_FAR_END, FROM_CALLER, FROM_FAR_END, FROM_KERNEL,
    PIECE_HAS_STATUS, PIECE_LAST, PRINCIPAL_REQUIRED, RECORD_AUDIT, RECORD_PUT, REFUSAL_GATE,
    ROUTE_DIRECT, ROUTE_LOCAL, ROUTE_ONCE, ROUTE_POOL, ROUTE_SCOPE, ROUTE_SESSION, ROUTE_STREAM,
    UNITS_REPORTED,
};
use serde_json::Value;

use busbar_plugin_loader::dispatch::kinds::plane::{OwnedSnapshot, Plane, ServedFacts};
use busbar_plugin_loader::dispatch::{now_ns, Called, Dispatcher, Frame, Lent, Plugin};

const CAP: usize = 8;

const OP_DEADLINE: Duration = Duration::from_secs(30);

fn deadline() -> u64 {
    now_ns().saturating_add(u64::try_from(OP_DEADLINE.as_nanos()).unwrap_or(u64::MAX))
}

fn text(v: &Value, what: &str) -> Vec<u8> {
    match v {
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Null => panic!("conformance.json: plane.{what} is missing"),
        other => other.to_string().into_bytes(),
    }
}

fn bytes(v: &Value, what: &str) -> Vec<u8> {
    match v {
        Value::Null => Vec::new(),
        Value::Array(parts) => parts
            .iter()
            .flat_map(|p| {
                if p.is_object() {
                    let times = usize::try_from(num(&p["times"], what)).unwrap_or(usize::MAX);
                    text(&p["repeat"], what).repeat(times)
                } else {
                    text(p, what)
                }
            })
            .collect(),
        other => text(other, what),
    }
}

fn num(v: &Value, what: &str) -> u64 {
    v.as_u64()
        .unwrap_or_else(|| panic!("conformance.json: plane.{what} must be a number"))
}

fn num_or(v: &Value, what: &str, or: u64) -> u64 {
    if v.is_null() {
        or
    } else {
        num(v, what)
    }
}

fn small(v: u64, what: &str) -> u32 {
    u32::try_from(v).unwrap_or_else(|_| panic!("conformance.json: plane.{what} is too large"))
}

fn pairs(v: &Value, what: &str) -> Vec<(String, String)> {
    v.as_array()
        .unwrap_or_else(|| panic!("conformance.json: plane.{what} must be an array of pairs"))
        .iter()
        .map(|p| match p.as_array().map(Vec::as_slice) {
            Some([a, b]) => (
                a.as_str().map_or_else(|| a.to_string(), String::from),
                b.as_str().map_or_else(|| b.to_string(), String::from),
            ),
            _ => panic!("conformance.json: plane.{what} holds a non-pair"),
        })
        .collect()
}

fn pairs_or_none(v: &Value, what: &str) -> Vec<(String, String)> {
    if v.is_null() {
        Vec::new()
    } else {
        pairs(v, what)
    }
}

fn field_list(pairs: &[(String, String)]) -> Vec<Field> {
    pairs
        .iter()
        .map(|(n, v)| Field {
            name: abi(n.as_bytes()),
            value: abi(v.as_bytes()),
        })
        .collect()
}

fn z<T: Copy>() -> T {
    // SAFETY: called only for the plane ABI's plain C structs (`UnitCount`, `RecordWrite`,
    // `OutField`, `SignalEntry`, `MessageView`): integers, spans, pointers and unions of integers,
    // for which all-zero is valid.
    unsafe { std::mem::zeroed() }
}

fn octets(b: &[u8]) -> Blob {
    Blob {
        ptr: b.as_ptr(),
        len: b.len(),
        fmt: BLOB_OCTETS,
        flags: 0,
    }
}

fn abi(b: &[u8]) -> AbiStr {
    AbiStr {
        ptr: b.as_ptr(),
        len: b.len(),
    }
}

fn read(s: AbiStr) -> String {
    if s.ptr.is_null() || s.len == 0 {
        return String::new();
    }
    // SAFETY: the kind's check admitted the answer; the plane's text is valid until the
    // instance's next call, and it is read before any.
    String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(s.ptr, s.len) }).into_owned()
}

fn at(buf: &[u8], s: Span) -> String {
    let (from, len) = (s.offset as usize, s.len as usize);
    buf.get(from..from + len).map_or_else(
        || "<OUTSIDE>".into(),
        |b| String::from_utf8_lossy(b).into_owned(),
    )
}

fn shown(buf: &[u8], s: Span) -> String {
    let (from, len) = (s.offset as usize, s.len as usize);
    let Some(b) = buf.get(from..from + len) else {
        return "<OUTSIDE>".into();
    };
    match std::str::from_utf8(b) {
        Ok(t) if !t.chars().any(char::is_control) => t.to_string(),
        _ => format!(
            "hex:{}",
            b.iter().map(|x| format!("{x:02x}")).collect::<String>()
        ),
    }
}

fn route_class(route: u8) -> String {
    match route {
        ROUTE_POOL => "pool".into(),
        ROUTE_DIRECT => "direct".into(),
        ROUTE_LOCAL => "local".into(),
        ROUTE_SCOPE => "scope".into(),
        other => other.to_string(),
    }
}

fn route_flags(flags: u8) -> Vec<String> {
    let mut named: Vec<String> = [
        (ROUTE_ONCE, "once"),
        (ROUTE_SESSION, "session"),
        (ROUTE_STREAM, "stream"),
    ]
    .iter()
    .filter(|(bit, _)| flags & bit != 0)
    .map(|(_, name)| (*name).to_string())
    .collect();
    let other = flags & !(ROUTE_ONCE | ROUTE_SESSION | ROUTE_STREAM);
    if other != 0 {
        named.push(other.to_string());
    }
    named
}

fn snapshot(s: Option<&OwnedSnapshot>) -> String {
    s.map_or_else(
        || "snapshot=none".into(),
        |s| {
            format!(
                "gen={} claims=[{}] routes={}",
                s.generation,
                claims(
                    s.claims
                        .iter()
                        .map(|c| (c.verb.as_str(), c.target.as_str()))
                ),
                s.admin_routes.len()
            )
        },
    )
}

fn claims<'a>(c: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    c.map(|(v, t)| format!("{v} {t}"))
        .collect::<Vec<_>>()
        .join(", ")
}

struct Piece {
    reply: Vec<u8>,
    units: [UnitCount; CAP],
    records: [RecordWrite; CAP],
    fields: [OutField; CAP],
    arena: [u8; 1024],
    bytes: Vec<u8>,
    _head: Vec<(String, String)>,
    head: Vec<Field>,
    member: Vec<u8>,
    pool: Option<Vec<u8>>,
    caller_ref: Option<Vec<u8>>,
}

// SAFETY: the raw pointers in `head` point into `_head`, owned by the same `Piece`; nothing
// mutates a lent `Piece` but the plane's writes into its buffers, which the host reads only after
// the op completes.
unsafe impl Send for Piece {}
// SAFETY: as above.
unsafe impl Sync for Piece {}

/// One piece handed to the plane.
#[derive(Clone, Copy)]
struct Given<'a> {
    unit: u64,
    /// A live session's stream; `0` for a request unit.
    stream: u64,
    /// The snapshot claim the unit arrived on.
    claim: u32,
    from: u32,
    flags: u32,
    bytes: &'a [u8],
    attempt_no: u32,
    /// With `PIECE_HAS_STATUS`: the far end's status and kept head fields.
    status: u32,
    head: &'a [(String, String)],
}

struct Carried {
    member: Vec<u8>,
    pool: Option<Vec<u8>>,
    caller_ref: Option<Vec<u8>>,
}

/// The host list a SHORT answer is met with at capacity 0.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Buffer {
    Units,
    Fields,
}

impl Buffer {
    fn of(v: &Value, what: &str) -> Option<Self> {
        match v.as_str() {
            None if v.is_null() => None,
            Some("units") => Some(Self::Units),
            Some("fields") => Some(Self::Fields),
            _ => panic!("conformance.json: plane.{what} must be \"units\" or \"fields\""),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Units => "units",
            Self::Fields => "fields",
        }
    }

    const fn caps(self) -> (usize, usize) {
        match self {
            Self::Units => (0, CAP),
            Self::Fields => (CAP, 0),
        }
    }

    fn needed(self, o: &OnPieceOut) -> u32 {
        match self {
            Self::Units => o.units_needed,
            Self::Fields => o.fields_needed,
        }
    }
}

impl Piece {
    fn lent(
        reply_cap: usize,
        g: Given<'_>,
        (units_cap, fields_cap): (usize, usize),
        c: &Carried,
    ) -> (Arc<Self>, Frame<OnPieceIn, OnPieceOut>) {
        let mut piece = Arc::new(Self {
            reply: vec![0; reply_cap],
            units: [z(); CAP],
            records: [z(); CAP],
            fields: [z(); CAP],
            arena: [0; 1024],
            bytes: g.bytes.to_vec(),
            _head: g.head.to_vec(),
            head: Vec::new(),
            member: c.member.clone(),
            pool: c.pool.clone(),
            caller_ref: c.caller_ref.clone(),
        });
        let me = Arc::get_mut(&mut piece).expect("a piece not yet lent is unshared");
        me.head = field_list(&me._head);
        let mut i: OnPieceIn = input();
        (i.unit, i.from, i.flags, i.bytes) = (g.unit, g.from, g.flags, octets(&me.bytes));
        (i.stream, i.claim) = (g.stream, g.claim);
        (i.attempt_no, i.member) = (g.attempt_no, abi(&me.member));
        if let (Some(pool), true) = (&me.pool, g.from == FROM_KERNEL && g.attempt_no > 0) {
            i.pool = abi(pool);
        }
        if let Some(r) = &me.caller_ref {
            i.caller_ref = abi(r);
        }
        if g.flags & PIECE_HAS_STATUS != 0 {
            i.status_code = g.status;
            (i.head_fields, i.head_fields_len) = (me.head.as_ptr(), me.head.len());
        }
        (i.reply_buf, i.reply_cap) = (me.reply.as_mut_ptr(), me.reply.len());
        (i.units_buf, i.units_cap) = (me.units.as_mut_ptr(), units_cap);
        (i.records_buf, i.records_cap) = (me.records.as_mut_ptr(), me.records.len());
        (i.fields_buf, i.fields_cap) = (me.fields.as_mut_ptr(), fields_cap);
        (i.arena_buf, i.arena_cap) = (me.arena.as_mut_ptr(), me.arena.len());
        (piece, Frame::new(i, output()))
    }

    fn fields(&self, o: &OnPieceOut) -> Vec<String> {
        self.fields[..(o.fields_written as usize).min(CAP)]
            .iter()
            .map(|f| format!("{}={}", at(&self.arena, f.name), at(&self.arena, f.value)))
            .collect()
    }

    fn units(&self, o: &OnPieceOut) -> Vec<String> {
        self.units[..(o.units_written as usize).min(CAP)]
            .iter()
            .map(|u| format!("{}:{}:{}", u.class, u.amount, u.source == UNITS_REPORTED))
            .collect()
    }

    fn emitted(&self, o: &OnPieceOut) -> String {
        let n = usize::try_from(o.emitted).map_or(self.reply.len(), |n| n.min(self.reply.len()));
        String::from_utf8_lossy(&self.reply[..n]).into_owned()
    }

    fn line(&self, c: &Called, o: &OnPieceOut) -> String {
        format!(
            "{} emitted={} more={} to_far_end={} done={} status={} verb={} target={} \
             fields={:?} units={:?}",
            called(c),
            self.emitted(o),
            o.more,
            o.flags & EMIT_TO_FAR_END != 0,
            o.flags & EMIT_DONE != 0,
            o.reply_status,
            at(&self.arena, o.verb),
            at(&self.arena, o.target),
            self.fields(o),
            self.units(o),
        )
    }

    fn ledger(&self, o: &OnPieceOut) -> String {
        let mut out = String::new();
        if o.lane.len != 0 {
            out.push_str(&format!(" lane={}", at(&self.arena, o.lane)));
        }
        let records: Vec<String> = self.records[..(o.records_written as usize).min(CAP)]
            .iter()
            .map(|r| {
                let op = match r.op {
                    RECORD_PUT => "put".to_string(),
                    RECORD_AUDIT => "audit".to_string(),
                    other => other.to_string(),
                };
                format!(
                    "{op}:{}:{}={}",
                    r.kind,
                    shown(&self.arena, r.key),
                    shown(&self.arena, r.value)
                )
            })
            .collect();
        if !records.is_empty() {
            out.push_str(&format!(" records={records:?}"));
        }
        out
    }

    fn session_line(&self, c: &Called, o: &OnPieceOut) -> String {
        format!(
            "{} emitted={} more={} to_far_end={} done={} status={} verb={} target={} need={}{} \
             fields={:?} units={:?}",
            called(c),
            self.emitted(o),
            o.more,
            o.flags & EMIT_TO_FAR_END != 0,
            o.flags & EMIT_DONE != 0,
            o.reply_status,
            at(&self.arena, o.verb),
            at(&self.arena, o.target),
            o.need,
            self.ledger(o),
            self.fields(o),
            self.units(o),
        )
    }
}

struct Unit {
    ticket: Ticket,
    reply_cap: usize,
}

impl Unit {
    fn mint(d: &Dispatcher, reply_cap: usize) -> Self {
        Self {
            ticket: d.mint(0).expect("a unit's ticket is minted"),
            reply_cap,
        }
    }

    fn end(self, d: &Dispatcher) {
        d.recycle(self.ticket);
    }
}

struct Pieced {
    called: Called,
    short: bool,
    piece: Arc<Piece>,
    frame: Option<Frame<OnPieceIn, OnPieceOut>>,
}

impl Pieced {
    fn out(&self) -> OnPieceOut {
        self.frame.as_ref().map_or_else(output, |f| f.out)
    }

    fn line(&self, line: fn(&Piece, &Called, &OnPieceOut) -> String) -> String {
        line(&self.piece, &self.called, &self.out())
    }
}

fn on_piece(
    p: &Plugin<Plane>,
    d: &Dispatcher,
    unit: &Unit,
    g: Given<'_>,
    caps: (usize, usize),
    c: &Carried,
) -> Pieced {
    let (piece, f) = Piece::lent(unit.reply_cap, g, caps, c);
    let done = submit_on(
        p,
        d,
        unit.ticket,
        slot::ON_PIECE,
        f,
        (DeadlineClass::Stream, deadline()),
        Arc::clone(&piece) as Lent,
        Duration::ZERO,
    );
    let called = answered(&done);
    Pieced {
        called,
        short: done.short,
        piece,
        frame: done.frame,
    }
}

fn short_then(
    p: &Plugin<Plane>,
    d: &Dispatcher,
    unit: &Unit,
    g: Given<'_>,
    buffer: Buffer,
    c: &Carried,
    line: fn(&Piece, &Called, &OnPieceOut) -> String,
) -> String {
    let first = on_piece(p, d, unit, g, buffer.caps(), c);
    let head = format!(
        "{} {}_needed={} recall={}",
        called(&first.called),
        buffer.name(),
        buffer.needed(&first.out()),
        first.short
    );
    let (true, Some(mut f)) = (first.short, first.frame) else {
        return format!("{head} then=no-recall");
    };
    (f.input.units_cap, f.input.fields_cap) = (CAP, CAP);
    let again = submit_on(
        p,
        d,
        unit.ticket,
        slot::ON_PIECE,
        f,
        (DeadlineClass::Stream, deadline()),
        Arc::clone(&first.piece) as Lent,
        Duration::ZERO,
    );
    let out = again.frame.as_ref().map_or_else(output, |f| f.out);
    format!(
        "{head} then={}",
        line(&first.piece, &answered(&again), &out)
    )
}

struct Arrival {
    unit: u64,
    claim: u32,
    method: Vec<u8>,
    target: Vec<u8>,
    fields: Vec<(String, String)>,

    route: bool,
}

impl Arrival {
    fn for_unit(&self, unit: u64) -> Self {
        Self {
            unit,
            claim: self.claim,
            method: self.method.clone(),
            target: self.target.clone(),
            fields: self.fields.clone(),
            route: self.route,
        }
    }

    fn of(a: &Value, what: &str) -> Self {
        Self {
            unit: num(&a["unit"], what),
            claim: small(num_or(&a["claim"], what, 0), what),
            method: text(&a["method"], what),
            target: text(&a["target"], what),
            fields: pairs_or_none(&a["fields"], what),
            route: a["route"].is_object(),
        }
    }
}

fn arrive(p: &Plugin<Plane>, a: &Arrival, body: &[u8]) -> String {
    let mut units = [z::<UnitCount>(); 4];
    let head = field_list(&a.fields);
    let mut f: Frame<ArriveIn, ArriveOut> = Frame::new(input(), output());
    (f.input.unit, f.input.claim) = (a.unit, a.claim);
    (f.input.method, f.input.target) = (abi(&a.method), abi(&a.target));
    if !head.is_empty() {
        (f.input.fields, f.input.fields_len) = (head.as_ptr(), head.len());
    }
    f.input.body = octets(body);
    (f.input.units_buf, f.input.units_cap) = (units.as_mut_ptr(), units.len());
    let c = p.call(slot::ARRIVE, &mut f);
    arrival_line(&c, &f.out, a.route)
}

fn arrival_line(c: &Called, out: &ArriveOut, route: bool) -> String {
    use busbar_contract::abi::mechanism::call::Outcome;
    if c.outcome == Outcome::Refused {
        return format!(
            "{} refusal={} status={}",
            called(c),
            out.refusal,
            out.refusal_status
        );
    }
    if c.outcome != Outcome::Ready {
        return format!("{} UNREAD", called(c));
    }
    let line = format!(
        "{} op_class={} principal_required={} dialect={} units={} refusal={} status={}",
        called(c),
        out.op_class,
        out.principal_need == PRINCIPAL_REQUIRED,
        out.dialect,
        out.units_written,
        out.refusal,
        out.refusal_status
    );
    if route {
        format!(
            "{line} route_flags={:?} route={} entry={}",
            route_flags(out.route_flags),
            route_class(out.route),
            read(out.pool)
        )
    } else {
        line
    }
}

fn drive(p: &Plugin<Plane>, driver: Option<Ticket>) -> (Called, Vec<u64>) {
    use busbar_contract::abi::mechanism::call::Outcome;
    let answer = |outcome, error: Option<&[u8]>| Called {
        outcome,
        error: error.map(<[u8]>::to_vec),
        lease: 0,
        recall: None,
    };
    let Some(driver) = driver else {
        return (
            answer(Outcome::Refused, Some(b"no driver ticket")),
            Vec::new(),
        );
    };
    let seen = p.test_driver_answers();
    busbar_plugin_loader::dispatch::ticket::host_wake(p.test_host_context(), driver);
    match p.test_driver_answer_after(seen, OP_DEADLINE) {
        Some(outcome) => (answer(outcome, None), p.test_driver_sessions()),
        None => (
            answer(Outcome::Fault, Some(b"the woken driver never answered")),
            Vec::new(),
        ),
    }
}

fn attempt(unit: u64, claim: u32) -> Given<'static> {
    Given {
        unit,
        stream: 0,
        claim,
        from: FROM_KERNEL,
        flags: 0,
        bytes: b"",
        attempt_no: 1,
        status: 0,
        head: &[],
    }
}

fn far_end<'a>(
    unit: u64,
    claim: u32,
    flags: u32,
    bytes: &'a [u8],
    (status, head): (u32, &'a [(String, String)]),
) -> Given<'a> {
    Given {
        unit,
        stream: 0,
        claim,
        from: FROM_FAR_END,
        flags,
        bytes,
        attempt_no: 0,
        status,
        head,
    }
}

fn handed(facts: &ServedFacts, settings: &[u8]) -> Vec<u8> {
    let keyed =
        facts.section == busbar_contract::section::RESERVED_POOLS_KEY || !facts.consumed.is_empty();
    if !keyed {
        return settings.to_vec();
    }
    let value = match serde_json::from_slice::<Value>(settings) {
        Ok(Value::Null) | Err(_) => return settings.to_vec(),
        Ok(value) => value,
    };
    let written_keyed = value.as_object().is_some_and(|o| {
        o.contains_key(facts.section)
            && o.keys()
                .all(|k| k == facts.section || facts.consumed.contains(&k.as_str()))
    });
    if written_keyed {
        return settings.to_vec();
    }
    serde_json::to_vec(&serde_json::json!({ facts.section: value })).expect("a keyed blob")
}

fn open(p: &Plugin<Plane>, settings: &[u8], public_url: Option<&[u8]>) -> String {
    let mut f: Frame<PlaneOpenIn, PlaneOpenOut> = Frame::new(input(), output());
    (f.input.open.generation, f.input.open.settings) = (1, json(settings));
    if let Some(url) = public_url {
        f.input.public_url = abi(url);
    }
    let (c, snap) = p.open(&mut f);
    format!("{} {}", called(&c), snapshot(snap.as_ref()))
}

fn generation(p: &Plugin<Plane>, s: u32, generation: u64) -> String {
    let mut g: Frame<GenIn, OutHead> = Frame::new(input(), output());
    g.input.generation = generation;
    called(&p.call(s, &mut g))
}

#[derive(Clone, Default)]
struct ServeFirst {
    route: u32,
    target: Vec<u8>,
    fields: Vec<(String, String)>,
    body: Vec<u8>,
}

struct ServeLend {
    request: ServeFirst,
    request_fields: Vec<Field>,
    reply: [u8; 512],
    arena: [u8; 256],
    fields: [OutField; 2],
}

// SAFETY: input pointers name immutable allocations owned here. Output buffers are written only by
// the crossing and read by the caller only when the dispatcher returns a joined frame.
unsafe impl Send for ServeLend {}
// SAFETY: the caller never inspects output buffers while an abandoned crossing can still run.
unsafe impl Sync for ServeLend {}

impl ServeLend {
    fn frame(request: Option<&ServeFirst>) -> (Arc<Self>, Frame<ServeIn, ServeOut>) {
        let mut lend = Arc::new(Self {
            request: request.cloned().unwrap_or_default(),
            request_fields: Vec::new(),
            reply: [0; 512],
            arena: [0; 256],
            fields: [z(); 2],
        });
        let me = Arc::get_mut(&mut lend).expect("unshared");
        me.request_fields = field_list(&me.request.fields);
        let mut f: Frame<ServeIn, ServeOut> = Frame::new(input(), output());
        if request.is_some() {
            f.input.route = me.request.route;
            f.input.target = abi(&me.request.target);
            (f.input.fields, f.input.fields_len) =
                (me.request_fields.as_ptr(), me.request_fields.len());
            f.input.body = octets(&me.request.body);
        }
        (f.input.reply_buf, f.input.reply_cap) = (me.reply.as_mut_ptr(), me.reply.len());
        (f.input.fields_buf, f.input.fields_cap) = (me.fields.as_mut_ptr(), me.fields.len());
        (f.input.arena_buf, f.input.arena_cap) = (me.arena.as_mut_ptr(), me.arena.len());
        (lend, f)
    }
}

fn serve(p: &Plugin<Plane>, d: &Arc<Dispatcher>, request: Option<&ServeFirst>) -> String {
    let (lend, f) = ServeLend::frame(request);
    let (c, f) = on_ticket_frame(
        p,
        d,
        slot::SERVE,
        f,
        DeadlineClass::Call,
        deadline(),
        lend as Lent,
    );
    let out = f.map_or_else(output, |f| f.out);
    format!(
        "{} status={} reply_written={}",
        called(&c),
        out.status,
        out.reply_written
    )
}

fn exercise(dropped_in: bool) -> Vec<(String, String)> {
    let mut config: Value = serde_json::from_str(include_str!("conformance.json")).unwrap();
    for session in config["plane"]["sessions"].as_array_mut().unwrap() {
        let unit = session["unit"].clone();
        let claim = session["claim"].as_u64().unwrap_or(0);
        for step in session["steps"].as_array_mut().unwrap() {
            if step["arrive"].is_object() {
                step["arrive"]["unit"] = unit.clone();
                step["arrive"]["claim"] = serde_json::json!(claim);
            }
        }
    }
    let k = &config["plane"];
    let settings = serde_json::to_vec(&config["settings"]).unwrap();
    let bad: Vec<Vec<u8>> = k["bad_settings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| text(x, "bad settings"))
        .collect();
    let refresh_settings = text(&k["refresh_settings"], "refresh settings");
    let claimed = Arrival::of(&k["claimed"], "claimed");
    let unclaimed = Arrival::of(&k["unclaimed"], "unclaimed");
    let request = text(&k["request"], "request");
    let far_answer = text(&k["far_end"]["answer"], "answer");
    let head = pairs(&k["far_end"]["fields"], "fields");
    let answer = (200, head.as_slice());
    let narrow = (9, 80, 2);
    let short = (11, Buffer::Units);
    let refusal = (403, b"denied".to_vec());
    let carried = Carried {
        member: b"claude".to_vec(),
        pool: None,
        caller_ref: None,
    };
    let c = &carried;
    let claim = claimed.claim;
    let url = None;
    let d = Arc::new(Dispatcher::new(
        DispatchConfig::default(),
        busbar_contract::services::test_caller_binding(),
    ));
    let p = load(dropped_in, &d);
    let probe = p.test_invocation_probe();
    let first = || {
        let (crossings, resumes) = probe();
        crossings - resumes
    };
    let billable = p.served().billable_classes;
    let mut answers = Vec::new();
    {
        let before = first();
        let answer = format!(
            "{:?} {} max_inflight={}",
            p.kind(),
            p.name(),
            p.max_inflight()
        );
        assert_eq!(first() - before, 0, "exact first crossings: {}", "facts");
        answers.push((("facts").to_string(), answer));
    };

    let section = p.served().section;
    let dealt = |b: &[u8]| -> Vec<u8> {
        let value: Value = serde_json::from_slice(b).expect("conformance.json settings are JSON");
        serde_json::to_vec(&serde_json::json!({ section: value })).expect("a dealt blob")
    };
    for (i, b) in bad.iter().enumerate() {
        {
            let before = first();
            let answer = called(&validate(&p, &dealt(b)));
            assert_eq!(
                first() - before,
                1,
                "exact first crossings: {}",
                &format!("validate bad #{i}")
            );
            answers.push((format!("validate bad #{i}").to_string(), answer));
        };
    }
    {
        let before = first();
        let answer = called(&validate(&p, &dealt(&settings)));
        assert_eq!(first() - before, 1, "exact first crossings: {}", "validate");
        answers.push((("validate").to_string(), answer));
    };

    {
        let before = first();
        let answer = arrive(&p, &claimed, &request);
        assert_eq!(
            first() - before,
            0,
            "exact first crossings: {}",
            "arrive unopened"
        );
        answers.push((("arrive unopened").to_string(), answer));
    };
    let opened = handed(&p.served(), &settings);
    {
        let before = first();
        let answer = open(&p, &opened, url);
        assert_eq!(first() - before, 1, "exact first crossings: {}", "open");
        answers.push((("open").to_string(), answer));
    };
    {
        let before = first();
        let ready = p.ready(&d, OP_DEADLINE);
        assert!(ready.is_ok(), "{ready:?}");
        assert_eq!(
            first() - before,
            u64::from(p.has_ready()),
            "ready crossings"
        );
        answers.push((
            "ready".into(),
            format!("has_ready={} {ready:?}", p.has_ready()),
        ));
    }

    {
        let before = first();
        let answer = open(&p, &opened, url);
        assert_eq!(
            first() - before,
            0,
            "exact first crossings: {}",
            "open again"
        );
        answers.push((("open again").to_string(), answer));
    };

    let driver = d.driver(&p, 0);
    {
        let before = first();
        let answer = generation(&p, slot::HYDRATE, 1);
        assert_eq!(first() - before, 1, "exact first crossings: {}", "hydrate");
        answers.push((("hydrate").to_string(), answer));
    };
    {
        let before = first();
        let answer = generation(&p, slot::START, 1);
        assert_eq!(first() - before, 1, "exact first crossings: {}", "start");
        answers.push((("start").to_string(), answer));
    };
    {
        let before = first();
        let answer = arrive(&p, &claimed, &request);
        assert_eq!(
            first() - before,
            1,
            "exact first crossings: {}",
            "arrive claimed"
        );
        answers.push((("arrive claimed").to_string(), answer));
    };
    {
        let before = first();
        let answer = arrive(&p, &unclaimed, &request);
        assert_eq!(
            first() - before,
            1,
            "exact first crossings: {}",
            "arrive unclaimed"
        );
        answers.push((("arrive unclaimed").to_string(), answer));
    };

    let unit = claimed.unit;
    let whole = PIECE_HAS_STATUS | PIECE_LAST;
    let room = (CAP, CAP);

    let one = Unit::mint(&d, 1024);
    for (label, g) in [
        ("attempt", attempt(unit, claim)),
        (
            "body",
            Given {
                from: FROM_CALLER,
                flags: PIECE_LAST,
                bytes: &request,
                attempt_no: 0,
                ..attempt(unit, claim)
            },
        ),
        ("far_end", far_end(unit, claim, whole, &far_answer, answer)),
    ] {
        {
            let before = first();
            let answer = on_piece(&p, &d, &one, g, room, c).line(Piece::line);
            assert_eq!(first() - before, 1, "exact first crossings: {}", label);
            answers.push(((label).to_string(), answer));
        };
    }
    one.end(&d);

    // Missing cache measurements stay absent (the main far_end case). Explicit
    // reported zeros are measurements and must survive the same actual door.
    {
        let zero = &k["reported_zero"];
        let zero_unit = num(&zero["unit"], "reported_zero.unit");
        let zero_body = text(&zero["answer"], "reported_zero.answer");
        let mut zero_arrival = Arrival::of(&k["claimed"], "claimed");
        zero_arrival.unit = zero_unit;
        let before = first();
        let arrived = arrive(&p, &zero_arrival, &request);
        assert_eq!(first() - before, 1, "reported zero arrival crossings");
        assert!(arrived.starts_with("Ready "), "reported zero: {arrived}");
        answers.push(("reported zero arrive".into(), arrived));
        let zero_ticket = Unit::mint(&d, 1024);
        for (label, given) in [
            ("reported zero attempt", attempt(zero_unit, claim)),
            (
                "reported zero body",
                Given {
                    from: FROM_CALLER,
                    flags: PIECE_LAST,
                    bytes: &request,
                    attempt_no: 0,
                    ..attempt(zero_unit, claim)
                },
            ),
            (
                "reported zero answer",
                far_end(zero_unit, claim, whole, &zero_body, answer),
            ),
        ] {
            let before = first();
            let line = on_piece(&p, &d, &zero_ticket, given, room, c).line(Piece::line);
            assert_eq!(first() - before, 1, "{label}: exact first crossings");
            assert!(line.starts_with("Ready "), "{label}: {line}");
            answers.push((label.into(), line));
        }
        zero_ticket.end(&d);
    }

    let (narrow_unit, cap, more) = narrow;
    let narrow = Unit::mint(&d, cap);

    {
        let before = first();
        let answer = arrive(&p, &claimed.for_unit(narrow_unit), &request);
        assert_eq!(
            first() - before,
            1,
            "exact first crossings: {}",
            "narrow arrive"
        );
        answers.push((("narrow arrive").to_string(), answer));
    };
    {
        let before = first();
        let answer =
            on_piece(&p, &d, &narrow, attempt(narrow_unit, claim), room, c).line(Piece::line);
        assert_eq!(
            first() - before,
            1,
            "exact first crossings: {}",
            "narrow attempt"
        );
        answers.push((("narrow attempt").to_string(), answer));
    };
    {
        let before = first();
        let answer = {
            let g = Given {
                from: FROM_CALLER,
                flags: PIECE_LAST,
                bytes: &request,
                attempt_no: 0,
                ..attempt(narrow_unit, claim)
            };
            on_piece(&p, &d, &narrow, g, room, c).line(Piece::line)
        };
        assert_eq!(
            first() - before,
            1,
            "exact first crossings: {}",
            "narrow body"
        );
        answers.push((("narrow body").to_string(), answer));
    };
    {
        let before = first();
        let answer = {
            let g = far_end(narrow_unit, claim, whole, &far_answer, answer);
            on_piece(&p, &d, &narrow, g, room, c).line(Piece::line)
        };
        assert_eq!(
            first() - before,
            1,
            "exact first crossings: {}",
            "narrow far_end"
        );
        answers.push((("narrow far_end").to_string(), answer));
    };
    for i in 0..more {
        {
            let before = first();
            let answer = {
                let g = far_end(narrow_unit, claim, 0, b"", answer);
                on_piece(&p, &d, &narrow, g, room, c).line(Piece::line)
            };
            assert_eq!(
                first() - before,
                1,
                "exact first crossings: {}",
                &format!("narrow more #{i}")
            );
            answers.push((format!("narrow more #{i}").to_string(), answer));
        };
    }
    narrow.end(&d);

    let (short_unit, buffer) = short;
    let short = Unit::mint(&d, 1024);
    {
        let before = first();
        let answer = arrive(&p, &claimed.for_unit(short_unit), &request);
        assert_eq!(
            first() - before,
            1,
            "exact first crossings: {}",
            "short arrive"
        );
        answers.push((("short arrive").to_string(), answer));
    };
    {
        let before = first();
        let answer =
            on_piece(&p, &d, &short, attempt(short_unit, claim), room, c).line(Piece::line);
        assert_eq!(
            first() - before,
            1,
            "exact first crossings: {}",
            "short attempt"
        );
        answers.push((("short attempt").to_string(), answer));
    };
    {
        let before = first();
        let answer = {
            let g = Given {
                from: FROM_CALLER,
                flags: PIECE_LAST,
                bytes: &request,
                attempt_no: 0,
                ..attempt(short_unit, claim)
            };
            on_piece(&p, &d, &short, g, room, c).line(Piece::line)
        };
        assert_eq!(
            first() - before,
            1,
            "exact first crossings: {}",
            "short body"
        );
        answers.push((("short body").to_string(), answer));
    };
    {
        let before = first();
        let answer = {
            let g = far_end(short_unit, claim, whole, &far_answer, answer);
            short_then(&p, &d, &short, g, buffer, c, Piece::line)
        };
        assert_eq!(
            first() - before,
            2,
            "exact first crossings: {}",
            "far_end short"
        );
        answers.push((("far_end short").to_string(), answer));
    };
    short.end(&d);

    {
        let unit = Unit::mint(&d, 65536);
        let own = Carried {
            member: b"claude".to_vec(),
            pool: None,
            caller_ref: None,
        };
        {
            let before = first();
            let answer = {
                let mut arrival =
                    Arrival::of(&k["sessions"][0]["steps"][0]["arrive"], "session arrival");
                arrival.unit = 20;
                arrival.claim = 0;
                arrive(
                    &p,
                    &arrival,
                    &bytes(&k["sessions"][0]["steps"][0]["arrive"]["body"], "body"),
                )
            };
            assert_eq!(
                first() - before,
                1,
                "anthropic server tools arrive: exact first crossings"
            );
            answers.push(("anthropic server tools arrive".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][0]["steps"][1]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][0]["steps"][1]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 20,
                        stream: 0,
                        claim: 0,
                        from: FROM_KERNEL,
                        flags: 0,
                        bytes: &payload,
                        attempt_no: 1,
                        status: 0,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "anthropic server tools attempt: exact first crossings"
            );
            answers.push(("anthropic server tools attempt".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][0]["steps"][2]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][0]["steps"][2]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 20,
                        stream: 0,
                        claim: 0,
                        from: FROM_CALLER,
                        flags: PIECE_LAST,
                        bytes: &payload,
                        attempt_no: 0,
                        status: 0,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "anthropic server tools body: exact first crossings"
            );
            answers.push(("anthropic server tools body".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][0]["steps"][3]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][0]["steps"][3]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 20,
                        stream: 0,
                        claim: 0,
                        from: FROM_FAR_END,
                        flags: PIECE_LAST | PIECE_HAS_STATUS,
                        bytes: &payload,
                        attempt_no: 0,
                        status: 200,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "anthropic server tools answer: exact first crossings"
            );
            answers.push(("anthropic server tools answer".into(), answer));
        }
        unit.end(&d);
    }

    {
        let unit = Unit::mint(&d, 65536);
        let own = Carried {
            member: b"gpt".to_vec(),
            pool: None,
            caller_ref: None,
        };
        {
            let before = first();
            let answer = {
                let mut arrival =
                    Arrival::of(&k["sessions"][1]["steps"][0]["arrive"], "session arrival");
                arrival.unit = 21;
                arrival.claim = 0;
                arrive(
                    &p,
                    &arrival,
                    &bytes(&k["sessions"][1]["steps"][0]["arrive"]["body"], "body"),
                )
            };
            assert_eq!(
                first() - before,
                1,
                "openai stated total arrive: exact first crossings"
            );
            answers.push(("openai stated total arrive".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][1]["steps"][1]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][1]["steps"][1]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 21,
                        stream: 0,
                        claim: 0,
                        from: FROM_KERNEL,
                        flags: 0,
                        bytes: &payload,
                        attempt_no: 1,
                        status: 0,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "openai stated total attempt: exact first crossings"
            );
            answers.push(("openai stated total attempt".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][1]["steps"][2]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][1]["steps"][2]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 21,
                        stream: 0,
                        claim: 0,
                        from: FROM_CALLER,
                        flags: PIECE_LAST,
                        bytes: &payload,
                        attempt_no: 0,
                        status: 0,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "openai stated total body: exact first crossings"
            );
            answers.push(("openai stated total body".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][1]["steps"][3]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][1]["steps"][3]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 21,
                        stream: 0,
                        claim: 0,
                        from: FROM_FAR_END,
                        flags: PIECE_LAST | PIECE_HAS_STATUS,
                        bytes: &payload,
                        attempt_no: 0,
                        status: 200,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "openai stated total answer: exact first crossings"
            );
            answers.push(("openai stated total answer".into(), answer));
        }
        unit.end(&d);
    }

    {
        let unit = Unit::mint(&d, 65536);
        let own = Carried {
            member: b"command".to_vec(),
            pool: None,
            caller_ref: None,
        };
        {
            let before = first();
            let answer = {
                let mut arrival =
                    Arrival::of(&k["sessions"][2]["steps"][0]["arrive"], "session arrival");
                arrival.unit = 22;
                arrival.claim = 0;
                arrive(
                    &p,
                    &arrival,
                    &bytes(&k["sessions"][2]["steps"][0]["arrive"]["body"], "body"),
                )
            };
            assert_eq!(
                first() - before,
                1,
                "cohere billed units arrive: exact first crossings"
            );
            answers.push(("cohere billed units arrive".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][2]["steps"][1]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][2]["steps"][1]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 22,
                        stream: 0,
                        claim: 0,
                        from: FROM_KERNEL,
                        flags: 0,
                        bytes: &payload,
                        attempt_no: 1,
                        status: 0,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "cohere billed units attempt: exact first crossings"
            );
            answers.push(("cohere billed units attempt".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][2]["steps"][2]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][2]["steps"][2]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 22,
                        stream: 0,
                        claim: 0,
                        from: FROM_CALLER,
                        flags: PIECE_LAST,
                        bytes: &payload,
                        attempt_no: 0,
                        status: 0,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "cohere billed units body: exact first crossings"
            );
            answers.push(("cohere billed units body".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][2]["steps"][3]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][2]["steps"][3]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 22,
                        stream: 0,
                        claim: 0,
                        from: FROM_FAR_END,
                        flags: PIECE_LAST | PIECE_HAS_STATUS,
                        bytes: &payload,
                        attempt_no: 0,
                        status: 200,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "cohere billed units answer: exact first crossings"
            );
            answers.push(("cohere billed units answer".into(), answer));
        }
        unit.end(&d);
    }

    {
        let unit = Unit::mint(&d, 65536);
        let own = Carried {
            member: b"nova".to_vec(),
            pool: None,
            caller_ref: None,
        };
        {
            let before = first();
            let answer = {
                let mut arrival =
                    Arrival::of(&k["sessions"][3]["steps"][0]["arrive"], "session arrival");
                arrival.unit = 23;
                arrival.claim = 0;
                arrive(
                    &p,
                    &arrival,
                    &bytes(&k["sessions"][3]["steps"][0]["arrive"]["body"], "body"),
                )
            };
            assert_eq!(
                first() - before,
                1,
                "bedrock guardrail a arrive: exact first crossings"
            );
            answers.push(("bedrock guardrail a arrive".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][3]["steps"][1]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][3]["steps"][1]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 23,
                        stream: 0,
                        claim: 0,
                        from: FROM_KERNEL,
                        flags: 0,
                        bytes: &payload,
                        attempt_no: 1,
                        status: 0,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "bedrock guardrail a attempt: exact first crossings"
            );
            answers.push(("bedrock guardrail a attempt".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][3]["steps"][2]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][3]["steps"][2]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 23,
                        stream: 0,
                        claim: 0,
                        from: FROM_CALLER,
                        flags: PIECE_LAST,
                        bytes: &payload,
                        attempt_no: 0,
                        status: 0,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "bedrock guardrail a body: exact first crossings"
            );
            answers.push(("bedrock guardrail a body".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][3]["steps"][3]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][3]["steps"][3]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 23,
                        stream: 0,
                        claim: 0,
                        from: FROM_FAR_END,
                        flags: PIECE_LAST | PIECE_HAS_STATUS,
                        bytes: &payload,
                        attempt_no: 0,
                        status: 200,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "bedrock guardrail a answer: exact first crossings"
            );
            answers.push(("bedrock guardrail a answer".into(), answer));
        }
        unit.end(&d);
    }

    {
        let unit = Unit::mint(&d, 65536);
        let own = Carried {
            member: b"nova".to_vec(),
            pool: None,
            caller_ref: None,
        };
        {
            let before = first();
            let answer = {
                let mut arrival =
                    Arrival::of(&k["sessions"][4]["steps"][0]["arrive"], "session arrival");
                arrival.unit = 24;
                arrival.claim = 0;
                arrive(
                    &p,
                    &arrival,
                    &bytes(&k["sessions"][4]["steps"][0]["arrive"]["body"], "body"),
                )
            };
            assert_eq!(
                first() - before,
                1,
                "bedrock guardrail b arrive: exact first crossings"
            );
            answers.push(("bedrock guardrail b arrive".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][4]["steps"][1]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][4]["steps"][1]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 24,
                        stream: 0,
                        claim: 0,
                        from: FROM_KERNEL,
                        flags: 0,
                        bytes: &payload,
                        attempt_no: 1,
                        status: 0,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "bedrock guardrail b attempt: exact first crossings"
            );
            answers.push(("bedrock guardrail b attempt".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][4]["steps"][2]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][4]["steps"][2]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 24,
                        stream: 0,
                        claim: 0,
                        from: FROM_CALLER,
                        flags: PIECE_LAST,
                        bytes: &payload,
                        attempt_no: 0,
                        status: 0,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "bedrock guardrail b body: exact first crossings"
            );
            answers.push(("bedrock guardrail b body".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][4]["steps"][3]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][4]["steps"][3]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 24,
                        stream: 0,
                        claim: 0,
                        from: FROM_FAR_END,
                        flags: PIECE_LAST | PIECE_HAS_STATUS,
                        bytes: &payload,
                        attempt_no: 0,
                        status: 200,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "bedrock guardrail b answer: exact first crossings"
            );
            answers.push(("bedrock guardrail b answer".into(), answer));
        }
        unit.end(&d);
    }

    {
        let unit = Unit::mint(&d, 65536);
        let own = Carried {
            member: b"nova".to_vec(),
            pool: None,
            caller_ref: None,
        };
        {
            let before = first();
            let answer = {
                let mut arrival =
                    Arrival::of(&k["sessions"][5]["steps"][0]["arrive"], "session arrival");
                arrival.unit = 25;
                arrival.claim = 0;
                arrive(
                    &p,
                    &arrival,
                    &bytes(&k["sessions"][5]["steps"][0]["arrive"]["body"], "body"),
                )
            };
            assert_eq!(
                first() - before,
                1,
                "bedrock guardrail c arrive: exact first crossings"
            );
            answers.push(("bedrock guardrail c arrive".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][5]["steps"][1]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][5]["steps"][1]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 25,
                        stream: 0,
                        claim: 0,
                        from: FROM_KERNEL,
                        flags: 0,
                        bytes: &payload,
                        attempt_no: 1,
                        status: 0,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "bedrock guardrail c attempt: exact first crossings"
            );
            answers.push(("bedrock guardrail c attempt".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][5]["steps"][2]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][5]["steps"][2]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 25,
                        stream: 0,
                        claim: 0,
                        from: FROM_CALLER,
                        flags: PIECE_LAST,
                        bytes: &payload,
                        attempt_no: 0,
                        status: 0,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "bedrock guardrail c body: exact first crossings"
            );
            answers.push(("bedrock guardrail c body".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][5]["steps"][3]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][5]["steps"][3]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 25,
                        stream: 0,
                        claim: 0,
                        from: FROM_FAR_END,
                        flags: PIECE_LAST | PIECE_HAS_STATUS,
                        bytes: &payload,
                        attempt_no: 0,
                        status: 200,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "bedrock guardrail c answer: exact first crossings"
            );
            answers.push(("bedrock guardrail c answer".into(), answer));
        }
        unit.end(&d);
    }

    {
        let unit = Unit::mint(&d, 65536);
        let own = Carried {
            member: b"dalle".to_vec(),
            pool: None,
            caller_ref: None,
        };
        {
            let before = first();
            let answer = {
                let mut arrival =
                    Arrival::of(&k["sessions"][6]["steps"][0]["arrive"], "session arrival");
                arrival.unit = 26;
                arrival.claim = 0;
                arrive(
                    &p,
                    &arrival,
                    &bytes(&k["sessions"][6]["steps"][0]["arrive"]["body"], "body"),
                )
            };
            assert_eq!(
                first() - before,
                1,
                "openai images arrive: exact first crossings"
            );
            answers.push(("openai images arrive".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][6]["steps"][1]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][6]["steps"][1]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 26,
                        stream: 0,
                        claim: 0,
                        from: FROM_KERNEL,
                        flags: 0,
                        bytes: &payload,
                        attempt_no: 1,
                        status: 0,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "openai images attempt: exact first crossings"
            );
            answers.push(("openai images attempt".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][6]["steps"][2]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][6]["steps"][2]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 26,
                        stream: 0,
                        claim: 0,
                        from: FROM_CALLER,
                        flags: PIECE_LAST,
                        bytes: &payload,
                        attempt_no: 0,
                        status: 0,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "openai images body: exact first crossings"
            );
            answers.push(("openai images body".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][6]["steps"][3]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][6]["steps"][3]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 26,
                        stream: 0,
                        claim: 0,
                        from: FROM_FAR_END,
                        flags: PIECE_LAST | PIECE_HAS_STATUS,
                        bytes: &payload,
                        attempt_no: 0,
                        status: 200,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "openai images answer: exact first crossings"
            );
            answers.push(("openai images answer".into(), answer));
        }
        unit.end(&d);
    }

    {
        let unit = Unit::mint(&d, 65536);
        let own = Carried {
            member: b"whisper".to_vec(),
            pool: None,
            caller_ref: None,
        };
        {
            let before = first();
            let answer = {
                let mut arrival =
                    Arrival::of(&k["sessions"][7]["steps"][0]["arrive"], "session arrival");
                arrival.unit = 27;
                arrival.claim = 0;
                arrive(
                    &p,
                    &arrival,
                    &bytes(&k["sessions"][7]["steps"][0]["arrive"]["body"], "body"),
                )
            };
            assert_eq!(
                first() - before,
                1,
                "openai transcription arrive: exact first crossings"
            );
            answers.push(("openai transcription arrive".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][7]["steps"][1]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][7]["steps"][1]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 27,
                        stream: 0,
                        claim: 0,
                        from: FROM_KERNEL,
                        flags: 0,
                        bytes: &payload,
                        attempt_no: 1,
                        status: 0,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "openai transcription attempt: exact first crossings"
            );
            answers.push(("openai transcription attempt".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][7]["steps"][2]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][7]["steps"][2]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 27,
                        stream: 0,
                        claim: 0,
                        from: FROM_CALLER,
                        flags: PIECE_LAST,
                        bytes: &payload,
                        attempt_no: 0,
                        status: 0,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "openai transcription body: exact first crossings"
            );
            answers.push(("openai transcription body".into(), answer));
        }
        {
            let before = first();
            let answer = {
                let payload = bytes(
                    &k["sessions"][7]["steps"][3]["piece"]["bytes"],
                    "piece bytes",
                );
                let fields = pairs_or_none(
                    &k["sessions"][7]["steps"][3]["piece"]["fields"],
                    "piece fields",
                );
                on_piece(
                    &p,
                    &d,
                    &unit,
                    Given {
                        unit: 27,
                        stream: 0,
                        claim: 0,
                        from: FROM_FAR_END,
                        flags: PIECE_LAST | PIECE_HAS_STATUS,
                        bytes: &payload,
                        attempt_no: 0,
                        status: 200,
                        head: &fields,
                    },
                    (CAP, CAP),
                    &own,
                )
                .line(Piece::session_line)
            };
            assert_eq!(
                first() - before,
                1,
                "openai transcription answer: exact first crossings"
            );
            answers.push(("openai transcription answer".into(), answer));
        }
        unit.end(&d);
    }

    {
        let before = first();
        let answer = {
            let (mut reply, mut arena) = ([0_u8; 512], [0_u8; 256]);
            let mut fields = [z::<OutField>(); 2];
            let mut f: Frame<RefusalIn, RefusalOut> = Frame::new(input(), output());
            (f.input.cause, f.input.status, f.input.text) =
                (REFUSAL_GATE, refusal.0, abi(&refusal.1));
            (f.input.reply_buf, f.input.reply_cap) = (reply.as_mut_ptr(), reply.len());
            (f.input.fields_buf, f.input.fields_cap) = (fields.as_mut_ptr(), fields.len());
            (f.input.arena_buf, f.input.arena_cap) = (arena.as_mut_ptr(), arena.len());
            let c = p.call(slot::REFUSAL, &mut f);
            let written: Vec<String> = fields[..(f.out.fields_written as usize).min(2)]
                .iter()
                .map(|x| format!("{}={}", at(&arena, x.name), at(&arena, x.value)))
                .collect();
            format!(
                "{} reply={} fields={written:?} status={}",
                called(&c),
                String::from_utf8_lossy(&reply[..(f.out.reply_written as usize).min(reply.len())]),
                f.out.status
            )
        };
        assert_eq!(first() - before, 1, "exact first crossings: {}", "refusal");
        answers.push((("refusal").to_string(), answer));
    };
    {
        let before = first();
        let answer = serve(&p, &d, None);
        assert_eq!(first() - before, 1, "exact first crossings: {}", "serve");
        answers.push((("serve").to_string(), answer));
    };
    {
        let before = first();
        let answer = {
            let mut arena = [0_u8; 256];
            let mut signals = [z::<SignalEntry>(); 4];
            let mut f: Frame<ProjectIn, ProjectOut> = Frame::new(input(), output());
            (f.input.claim, f.input.target) = (claim, abi(&claimed.target));
            f.input.body = octets(&request);
            (f.input.signals_buf, f.input.signals_cap) = (signals.as_mut_ptr(), signals.len());
            (f.input.arena_buf, f.input.arena_cap) = (arena.as_mut_ptr(), arena.len());
            let c = p.call(slot::PROJECT, &mut f);
            format!("{} signals={}", called(&c), f.out.view.signals_len)
        };
        assert_eq!(first() - before, 1, "exact first crossings: {}", "project");
        answers.push((("project").to_string(), answer));
    };
    {
        let before = first();
        let answer = {
            let (c, named) = drive(&p, driver);
            format!("{} sessions={}", called(&c), named.len())
        };
        assert_eq!(first() - before, 1, "exact first crossings: {}", "drive");
        answers.push((("drive").to_string(), answer));
    };

    {
        let before = first();
        let answer = called(&release(&p, 0));
        assert_eq!(
            first() - before,
            1,
            "exact first crossings: {}",
            "release none"
        );
        answers.push((("release none").to_string(), answer));
    };
    {
        let before = first();
        let answer = match driver {
            Some(driver) => {
                let done = d.tick(&p, driver, 1_000).wait_done();
                let next = done.frame.as_ref().map_or(0, |f| f.out.next_tick_ns);
                format!("{} next={next}", called(&answered(&done)))
            }
            None => "Refused lease=false no driver ticket next=0".to_string(),
        };
        assert_eq!(first() - before, 1, "exact first crossings: {}", "tick");
        answers.push((("tick").to_string(), answer));
    };
    {
        let before = first();
        let answer = {
            let mut f: Frame<
                busbar_contract::abi::plane::PlaneCancelIn,
                busbar_contract::abi::plane::PlaneCancelOut,
            > = Frame::new(input(), output());
            let c = p.call(life::CANCEL, &mut f);
            format!("{} disposition={}", called(&c), f.out.cancel.disposition)
        };
        assert_eq!(first() - before, 1, "exact first crossings: {}", "cancel");
        answers.push((("cancel").to_string(), answer));
    };

    let refresh = |what: &[u8]| {
        let mut f: Frame<RefreshIn, PlaneRefreshOut> = Frame::new(input(), output());
        (f.input.generation, f.input.settings) = (2, json(what));
        let (c, snap) = p.refresh(&mut f);
        format!("{} {}", called(&c), snapshot(snap.as_ref()))
    };
    {
        let before = first();
        let answer = refresh(&handed(&p.served(), &refresh_settings));
        assert_eq!(first() - before, 1, "exact first crossings: {}", "refresh");
        answers.push((("refresh").to_string(), answer));
    };
    {
        let before = first();
        let answer = generation(&p, life::RETIRE, 1);
        assert_eq!(first() - before, 1, "exact first crossings: {}", "retire");
        answers.push((("retire").to_string(), answer));
    };
    {
        let before = first();
        let answer = called(&close(&p));
        assert_eq!(first() - before, 1, "exact first crossings: {}", "close");
        answers.push((("close").to_string(), answer));
    };

    {
        let before = first();
        let answer = arrive(&p, &claimed, &request);
        assert_eq!(
            first() - before,
            0,
            "exact first crossings: {}",
            "arrive after close"
        );
        answers.push((("arrive after close").to_string(), answer));
    };
    if let Some(driver) = driver {
        d.recycle(driver);
    }

    contract(&answers, k);
    let unreported = unreported_classes(&answers, &billable);
    assert!(unreported.is_empty(), "unreported classes: {unreported:?}");
    answers
}
fn answer<'f>(fold: &'f [(String, String)], label: &str) -> &'f str {
    fold.iter()
        .find(|s| s.0 == label)
        .map(|s| s.1.as_str())
        .unwrap_or_else(|| panic!("the script ran no step '{label}'"))
}

fn reported(v: &Value, what: &str) -> String {
    let units: Vec<String> = pairs(v, what)
        .iter()
        .map(|(class, amount)| format!("{class}:{amount}:true"))
        .collect();
    format!("units={units:?}")
}

fn release_none_contract(answer: &str) {
    assert!(
        matches!(
            answer,
            "Ready lease=false " | "Refused lease=false release: no such lease"
        ),
        "release none: {answer}"
    );
}

fn contract(fold: &[(String, String)], k: &Value) {
    let at = |label: &str| answer(fold, label);
    let str_of = |v: &Value, what: &str| String::from_utf8(text(v, what)).expect("UTF-8 input");
    for s in fold {
        if s.0 != "arrive after close" {
            assert!(!s.1.starts_with("Fault "), "{}: {}", s.0, s.1);
        }
    }
    let request_unit = k["request_unit"].as_bool() != Some(false);
    let units: &[&str] = if request_unit {
        &[
            "attempt",
            "body",
            "far_end",
            "narrow attempt",
            "narrow far_end",
            "short attempt",
        ]
    } else {
        &[]
    };
    for label in units {
        assert!(at(label).starts_with("Ready "), "{label}: {}", at(label));
    }
    for label in [
        "validate",
        "open",
        "hydrate",
        "start",
        "arrive claimed",
        "refusal",
        "drive",
        "tick",
        "cancel",
        "refresh",
        "retire",
        "close",
    ] {
        assert!(at(label).starts_with("Ready "), "{label}: {}", at(label));
    }
    release_none_contract(at("release none"));
    for (i, _) in k["bad_settings"]
        .as_array()
        .expect("conformance.json: bad_settings must be an array")
        .iter()
        .enumerate()
    {
        let refused = at(&format!("validate bad #{i}"));
        assert!(
            !refused.starts_with("Ready ") && refused.len() > "Refused lease=false ".len(),
            "a refused validate names why: {refused}"
        );
    }
    assert!(
        at("arrive unopened").starts_with("Refused "),
        "{}",
        at("arrive unopened")
    );
    assert!(
        at("open again").starts_with("Refused "),
        "one open per instance: {}",
        at("open again")
    );
    assert!(
        at("arrive after close").starts_with("Fault "),
        "{}",
        at("arrive after close")
    );

    let want_claims = |what: &str| {
        let c = pairs(&k[what], what);
        claims(c.iter().map(|(v, t)| (v.as_str(), t.as_str())))
    };
    assert!(
        at("open").contains(&format!("gen=1 claims=[{}] ", want_claims("open_claims"))),
        "open: {}",
        at("open")
    );
    assert!(
        at("refresh").contains(&format!(
            "gen=2 claims=[{}] ",
            want_claims("refresh_claims")
        )),
        "refresh: {}",
        at("refresh")
    );

    if let Some(route) = k["claimed"]["route"].as_object() {
        let want = format!(
            " route={} entry={}",
            str_of(&route["class"], "claimed.route.class"),
            str_of(&route["entry"], "claimed.route.entry")
        );
        assert!(
            at("arrive claimed").ends_with(&want),
            "a claimed request routes over{want}: {}",
            at("arrive claimed")
        );
        if let Some(flags) = route["flags"].as_array() {
            let flags: Vec<String> = flags
                .iter()
                .map(|f| str_of(f, "claimed.route.flags[]"))
                .collect();
            let want = format!(" route_flags={flags:?} ");
            assert!(
                at("arrive claimed").contains(&want),
                "a claimed request states{want}: {}",
                at("arrive claimed")
            );
        }
    }
    let status = num(&k["unclaimed"]["status"], "unclaimed.status");
    let unclaimed = at("arrive unclaimed");
    assert!(
        !unclaimed.starts_with("Ready ") && unclaimed.ends_with(&format!(" status={status}")),
        "an unclaimed request is refused at {status}: {unclaimed}"
    );

    if request_unit {
        attempts_and_bodies(fold, k);

        let f = &k["far_end"];
        let answer_out = str_of(&f["answer_out"], "far_end.answer_out");
        let units = reported(&f["units"], "far_end.units");
        let fs = num(&f["status"], "far_end.status");
        let whole = at("far_end");
        assert!(
            whole.contains(&format!(
                " emitted={answer_out} more=0 to_far_end=false done=true status={fs} "
            )) && whole.ends_with(&units),
            "the far end's whole answer, relayed with its count ({units}): {whole}"
        );

        let zero = &k["reported_zero"];
        let zero_body = str_of(&zero["answer"], "reported_zero.answer");
        let zero_units = reported(&zero["units"], "reported_zero.units");
        let zero_answer = at("reported zero answer");
        assert!(
            zero_answer.contains(&format!(
                " emitted={zero_body} more=0 to_far_end=false done=true status=200 "
            )) && zero_answer.ends_with(&zero_units),
            "explicit reported cache zeros survive exactly ({zero_units}): {zero_answer}"
        );

        let more = num(&k["narrow"]["more"], "narrow.more");
        let mut pieces = vec![at("narrow far_end").to_string()];
        pieces.extend((0..more).map(|i| at(&format!("narrow more #{i}")).to_string()));
        let mut relayed = String::new();
        for (i, line) in pieces.iter().enumerate() {
            let last = i + 1 == pieces.len();
            let emitted = line
                .split_once(" emitted=")
                .and_then(|(_, rest)| rest.split_once(" more="))
                .map(|(e, _)| e)
                .unwrap_or_else(|| panic!("narrow piece #{i}: {line}"));
            relayed.push_str(emitted);
            let tail = if last {
                " more=0 to_far_end=false done=true "
            } else {
                " more=1 to_far_end=false done=false "
            };
            assert!(
                line.starts_with("Ready ") && line.contains(tail),
                "narrow piece #{i}: {line}"
            );
        }
        assert_eq!(
            relayed, answer_out,
            "the narrow pieces relay the whole answer"
        );
        assert!(
            pieces[0].ends_with(&units),
            "the count rides the first narrow write: {}",
            pieces[0]
        );

        let short = at("far_end short");
        let buffer = Buffer::of(&k["short"]["buffer"], "short.buffer").unwrap_or(Buffer::Units);
        assert!(
            short.starts_with("Failed ")
                && !short.contains(&format!(" {}_needed=0 ", buffer.name()))
                && short.contains(" recall=true then=Ready ")
                && short.contains(&format!(
                    " emitted={answer_out} more=0 to_far_end=false done=true "
                ))
                && short.ends_with(&units),
            "a short answer, re-called once with room: {short}"
        );

        if k["empty"].is_object() {
            let want = str_of(&k["empty"]["outcome"], "empty.outcome");
            assert!(
                at("empty").starts_with(&format!("{want} ")),
                "empty: {}",
                at("empty")
            );
        }
    }
    sessions_contract(fold);
    if let Some(reply) = k["refusal"]["reply"].as_str() {
        assert!(
            at("refusal").contains(&format!(" reply={reply} ")),
            "refusal: {}",
            at("refusal")
        );
    }
}

fn attempts_and_bodies(fold: &[(String, String)], k: &Value) {
    let at = |label: &str| answer(fold, label);
    let str_of = |v: &Value, what: &str| String::from_utf8(text(v, what)).expect("UTF-8 input");
    let far = k["attempt"]["to_far_end"].as_bool().unwrap_or(true);
    for label in ["attempt", "narrow attempt", "short attempt"] {
        assert!(
            at(label).contains(&format!(" to_far_end={far} ")),
            "{label}: {}",
            at(label)
        );
    }
    let request_out = str_of(&k["request_out"], "request_out");
    let mut bodies = vec!["body"];
    if k["arrive_each"].as_bool() == Some(true) {
        bodies.extend(["narrow body", "short body"]);
        for label in ["narrow arrive", "short arrive"] {
            assert!(at(label).starts_with("Ready "), "{label}: {}", at(label));
        }
    }
    for label in bodies {
        assert!(
            at(label).contains(&format!(" emitted={request_out} more=0 to_far_end=true ")),
            "{label}: {}",
            at(label)
        );
    }
}

fn sessions_contract(fold: &[(String, String)]) {
    {
        let line = answer(fold, "anthropic server tools arrive");
        assert!(
            line.starts_with("Ready "),
            "anthropic server tools arrive: {line}"
        );
    }
    {
        let line = answer(fold, "anthropic server tools attempt");
        assert!(
            line.starts_with("Ready "),
            "anthropic server tools attempt: {line}"
        );
        assert!(
            line.contains(" to_far_end=true "),
            "anthropic server tools attempt: {line}"
        );
        assert!(
            line.contains(" verb=POST "),
            "anthropic server tools attempt: {line}"
        );
    }
    {
        let line = answer(fold, "anthropic server tools body");
        assert!(
            line.starts_with("Ready "),
            "anthropic server tools body: {line}"
        );
        assert!(
            line.contains(" to_far_end=true "),
            "anthropic server tools body: {line}"
        );
        assert!(
            line.contains(" more=0 "),
            "anthropic server tools body: {line}"
        );
    }
    {
        let line = answer(fold, "anthropic server tools answer");
        assert!(
            line.starts_with("Ready "),
            "anthropic server tools answer: {line}"
        );
        assert!(
            line.contains(" to_far_end=false "),
            "anthropic server tools answer: {line}"
        );
        assert!(
            line.contains(" done=true "),
            "anthropic server tools answer: {line}"
        );
        assert!(
            line.contains(" status=200 "),
            "anthropic server tools answer: {line}"
        );
        assert!(
            line.ends_with(&reported(
                &serde_json::json!([[0, 7], [1, 3], [2, 2], [3, 5], [4, 4], [6, 6], [19, 1]]),
                "units"
            )),
            "anthropic server tools answer: {line}"
        );
    }
    {
        let line = answer(fold, "openai stated total arrive");
        assert!(
            line.starts_with("Ready "),
            "openai stated total arrive: {line}"
        );
    }
    {
        let line = answer(fold, "openai stated total attempt");
        assert!(
            line.starts_with("Ready "),
            "openai stated total attempt: {line}"
        );
        assert!(
            line.contains(" to_far_end=true "),
            "openai stated total attempt: {line}"
        );
        assert!(
            line.contains(" verb=POST "),
            "openai stated total attempt: {line}"
        );
    }
    {
        let line = answer(fold, "openai stated total body");
        assert!(
            line.starts_with("Ready "),
            "openai stated total body: {line}"
        );
        assert!(
            line.contains(" to_far_end=true "),
            "openai stated total body: {line}"
        );
        assert!(
            line.contains(" more=0 "),
            "openai stated total body: {line}"
        );
    }
    {
        let line = answer(fold, "openai stated total answer");
        assert!(
            line.starts_with("Ready "),
            "openai stated total answer: {line}"
        );
        assert!(
            line.contains(" to_far_end=false "),
            "openai stated total answer: {line}"
        );
        assert!(
            line.contains(" done=true "),
            "openai stated total answer: {line}"
        );
        assert!(
            line.contains(" status=200 "),
            "openai stated total answer: {line}"
        );
        assert!(
            line.ends_with(&reported(
                &serde_json::json!([[0, 10], [1, 4], [7, 6], [19, 1]]),
                "units"
            )),
            "openai stated total answer: {line}"
        );
    }
    {
        let line = answer(fold, "cohere billed units arrive");
        assert!(
            line.starts_with("Ready "),
            "cohere billed units arrive: {line}"
        );
    }
    {
        let line = answer(fold, "cohere billed units attempt");
        assert!(
            line.starts_with("Ready "),
            "cohere billed units attempt: {line}"
        );
        assert!(
            line.contains(" to_far_end=true "),
            "cohere billed units attempt: {line}"
        );
        assert!(
            line.contains(" verb=POST "),
            "cohere billed units attempt: {line}"
        );
    }
    {
        let line = answer(fold, "cohere billed units body");
        assert!(
            line.starts_with("Ready "),
            "cohere billed units body: {line}"
        );
        assert!(
            line.contains(" to_far_end=true "),
            "cohere billed units body: {line}"
        );
        assert!(
            line.contains(" more=0 "),
            "cohere billed units body: {line}"
        );
    }
    {
        let line = answer(fold, "cohere billed units answer");
        assert!(
            line.starts_with("Ready "),
            "cohere billed units answer: {line}"
        );
        assert!(
            line.contains(" to_far_end=false "),
            "cohere billed units answer: {line}"
        );
        assert!(
            line.contains(" done=true "),
            "cohere billed units answer: {line}"
        );
        assert!(
            line.contains(" status=200 "),
            "cohere billed units answer: {line}"
        );
        assert!(
            line.ends_with(&reported(
                &serde_json::json!([[0, 5], [1, 2], [4, 1], [5, 3], [19, 1]]),
                "units"
            )),
            "cohere billed units answer: {line}"
        );
    }
    {
        let line = answer(fold, "bedrock guardrail a arrive");
        assert!(
            line.starts_with("Ready "),
            "bedrock guardrail a arrive: {line}"
        );
    }
    {
        let line = answer(fold, "bedrock guardrail a attempt");
        assert!(
            line.starts_with("Ready "),
            "bedrock guardrail a attempt: {line}"
        );
        assert!(
            line.contains(" to_far_end=true "),
            "bedrock guardrail a attempt: {line}"
        );
        assert!(
            line.contains(" verb=POST "),
            "bedrock guardrail a attempt: {line}"
        );
    }
    {
        let line = answer(fold, "bedrock guardrail a body");
        assert!(
            line.starts_with("Ready "),
            "bedrock guardrail a body: {line}"
        );
        assert!(
            line.contains(" to_far_end=true "),
            "bedrock guardrail a body: {line}"
        );
        assert!(
            line.contains(" more=0 "),
            "bedrock guardrail a body: {line}"
        );
    }
    {
        let line = answer(fold, "bedrock guardrail a answer");
        assert!(
            line.starts_with("Ready "),
            "bedrock guardrail a answer: {line}"
        );
        assert!(
            line.contains(" to_far_end=false "),
            "bedrock guardrail a answer: {line}"
        );
        assert!(
            line.contains(" done=true "),
            "bedrock guardrail a answer: {line}"
        );
        assert!(
            line.contains(" status=200 "),
            "bedrock guardrail a answer: {line}"
        );
        assert!(
            line.ends_with(&reported(
                &serde_json::json!([[0, 7], [1, 3], [10, 1], [11, 2], [12, 3], [19, 1]]),
                "units"
            )),
            "bedrock guardrail a answer: {line}"
        );
    }
    {
        let line = answer(fold, "bedrock guardrail b arrive");
        assert!(
            line.starts_with("Ready "),
            "bedrock guardrail b arrive: {line}"
        );
    }
    {
        let line = answer(fold, "bedrock guardrail b attempt");
        assert!(
            line.starts_with("Ready "),
            "bedrock guardrail b attempt: {line}"
        );
        assert!(
            line.contains(" to_far_end=true "),
            "bedrock guardrail b attempt: {line}"
        );
        assert!(
            line.contains(" verb=POST "),
            "bedrock guardrail b attempt: {line}"
        );
    }
    {
        let line = answer(fold, "bedrock guardrail b body");
        assert!(
            line.starts_with("Ready "),
            "bedrock guardrail b body: {line}"
        );
        assert!(
            line.contains(" to_far_end=true "),
            "bedrock guardrail b body: {line}"
        );
        assert!(
            line.contains(" more=0 "),
            "bedrock guardrail b body: {line}"
        );
    }
    {
        let line = answer(fold, "bedrock guardrail b answer");
        assert!(
            line.starts_with("Ready "),
            "bedrock guardrail b answer: {line}"
        );
        assert!(
            line.contains(" to_far_end=false "),
            "bedrock guardrail b answer: {line}"
        );
        assert!(
            line.contains(" done=true "),
            "bedrock guardrail b answer: {line}"
        );
        assert!(
            line.contains(" status=200 "),
            "bedrock guardrail b answer: {line}"
        );
        assert!(
            line.ends_with(&reported(
                &serde_json::json!([[0, 7], [1, 3], [13, 4], [14, 5], [15, 6], [19, 1]]),
                "units"
            )),
            "bedrock guardrail b answer: {line}"
        );
    }
    {
        let line = answer(fold, "bedrock guardrail c arrive");
        assert!(
            line.starts_with("Ready "),
            "bedrock guardrail c arrive: {line}"
        );
    }
    {
        let line = answer(fold, "bedrock guardrail c attempt");
        assert!(
            line.starts_with("Ready "),
            "bedrock guardrail c attempt: {line}"
        );
        assert!(
            line.contains(" to_far_end=true "),
            "bedrock guardrail c attempt: {line}"
        );
        assert!(
            line.contains(" verb=POST "),
            "bedrock guardrail c attempt: {line}"
        );
    }
    {
        let line = answer(fold, "bedrock guardrail c body");
        assert!(
            line.starts_with("Ready "),
            "bedrock guardrail c body: {line}"
        );
        assert!(
            line.contains(" to_far_end=true "),
            "bedrock guardrail c body: {line}"
        );
        assert!(
            line.contains(" more=0 "),
            "bedrock guardrail c body: {line}"
        );
    }
    {
        let line = answer(fold, "bedrock guardrail c answer");
        assert!(
            line.starts_with("Ready "),
            "bedrock guardrail c answer: {line}"
        );
        assert!(
            line.contains(" to_far_end=false "),
            "bedrock guardrail c answer: {line}"
        );
        assert!(
            line.contains(" done=true "),
            "bedrock guardrail c answer: {line}"
        );
        assert!(
            line.contains(" status=200 "),
            "bedrock guardrail c answer: {line}"
        );
        assert!(
            line.ends_with(&reported(
                &serde_json::json!([[0, 7], [1, 3], [16, 7], [17, 8], [18, 9], [19, 1]]),
                "units"
            )),
            "bedrock guardrail c answer: {line}"
        );
    }
    {
        let line = answer(fold, "openai images arrive");
        assert!(line.starts_with("Ready "), "openai images arrive: {line}");
    }
    {
        let line = answer(fold, "openai images attempt");
        assert!(line.starts_with("Ready "), "openai images attempt: {line}");
        assert!(
            line.contains(" to_far_end=true "),
            "openai images attempt: {line}"
        );
        assert!(
            line.contains(" verb=POST "),
            "openai images attempt: {line}"
        );
    }
    {
        let line = answer(fold, "openai images body");
        assert!(line.starts_with("Ready "), "openai images body: {line}");
        assert!(
            line.contains(" to_far_end=true "),
            "openai images body: {line}"
        );
        assert!(line.contains(" more=0 "), "openai images body: {line}");
    }
    {
        let line = answer(fold, "openai images answer");
        assert!(line.starts_with("Ready "), "openai images answer: {line}");
        assert!(
            line.contains(" to_far_end=false "),
            "openai images answer: {line}"
        );
        assert!(line.contains(" done=true "), "openai images answer: {line}");
        assert!(
            line.contains(" status=200 "),
            "openai images answer: {line}"
        );
        assert!(
            line.ends_with(&reported(&serde_json::json!([[8, 2], [19, 1]]), "units")),
            "openai images answer: {line}"
        );
    }
    {
        let line = answer(fold, "openai transcription arrive");
        assert!(
            line.starts_with("Ready "),
            "openai transcription arrive: {line}"
        );
    }
    {
        let line = answer(fold, "openai transcription attempt");
        assert!(
            line.starts_with("Ready "),
            "openai transcription attempt: {line}"
        );
        assert!(
            line.contains(" to_far_end=true "),
            "openai transcription attempt: {line}"
        );
        assert!(
            line.contains(" verb=POST "),
            "openai transcription attempt: {line}"
        );
    }
    {
        let line = answer(fold, "openai transcription body");
        assert!(
            line.starts_with("Ready "),
            "openai transcription body: {line}"
        );
        assert!(
            line.contains(" to_far_end=true "),
            "openai transcription body: {line}"
        );
        assert!(
            line.contains(" more=0 "),
            "openai transcription body: {line}"
        );
    }
    {
        let line = answer(fold, "openai transcription answer");
        assert!(
            line.starts_with("Ready "),
            "openai transcription answer: {line}"
        );
        assert!(
            line.contains(" to_far_end=false "),
            "openai transcription answer: {line}"
        );
        assert!(
            line.contains(" done=true "),
            "openai transcription answer: {line}"
        );
        assert!(
            line.contains(" status=200 "),
            "openai transcription answer: {line}"
        );
        assert!(
            line.ends_with(&reported(&serde_json::json!([[9, 3000], [19, 1]]), "units")),
            "openai transcription answer: {line}"
        );
    }
}
fn unreported_classes<'c>(fold: &[(String, String)], billable: &[&'c str]) -> Vec<&'c str> {
    let reported: std::collections::BTreeSet<usize> =
        fold.iter().flat_map(|st| reported_indices(&st.1)).collect();
    billable
        .iter()
        .enumerate()
        .filter(|(i, _)| !reported.contains(i))
        .map(|(_, c)| *c)
        .collect()
}

fn reported_indices(line: &str) -> Vec<usize> {
    let Some(at) = line.find("units=[") else {
        return Vec::new();
    };
    let list = &line[at + "units=[".len()..];
    let list = &list[..list.find(']').unwrap_or(list.len())];
    list.split(',')
        .filter_map(|e| e.trim().trim_matches('"').split(':').next()?.parse().ok())
        .collect()
}

fn input<T: InFrame>() -> T {
    // SAFETY: every kind's `in` is plain integers, raw pointers, `Option<fn>` and nested plain
    // structs, for which the all-zero pattern is valid; `T` leads with an `InHead`, written below.
    let mut v: T = unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
    // SAFETY: `T: InFrame` leads with an `InHead`.
    unsafe { std::ptr::addr_of_mut!(v).cast::<InHead>().write(in_head()) };
    v
}

#[must_use]
fn output<T: OutFrame>() -> T {
    // SAFETY: as `input`, with an `OutHead` first.
    let mut v: T = unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
    // SAFETY: `T: OutFrame` leads with an `OutHead`.
    unsafe {
        std::ptr::addr_of_mut!(v)
            .cast::<OutHead>()
            .write(out_head())
    };
    v
}

#[must_use]
fn json(bytes: &[u8]) -> Blob {
    Blob {
        ptr: bytes.as_ptr(),
        len: bytes.len(),
        fmt: BLOB_JSON,
        flags: 0,
    }
}

#[must_use]
fn called(c: &Called) -> String {
    let text = c
        .error
        .as_deref()
        .map(String::from_utf8_lossy)
        .unwrap_or_default();
    format!("{:?} lease={} {text}", c.outcome, c.lease != 0)
}

fn on_ticket_frame<I: InFrame, O: OutFrame>(
    p: &Plugin<Plane>,
    d: &Dispatcher,
    s: u32,
    f: Frame<I, O>,
    class: DeadlineClass,
    deadline_ns: u64,
    lent: Lent,
) -> (Called, Option<Frame<I, O>>) {
    let Some(ticket) = d.mint(0) else {
        let refused = Called {
            outcome: Outcome::Refused,
            error: None,
            lease: 0,
            recall: None,
        };
        return (refused, None);
    };
    let done = submit_on(
        p,
        d,
        ticket,
        s,
        f,
        (class, deadline_ns),
        lent,
        Duration::ZERO,
    );
    d.recycle(ticket);
    (answered(&done), done.frame)
}

#[allow(clippy::too_many_arguments)]
fn submit_on<I: InFrame, O: OutFrame>(
    p: &Plugin<Plane>,
    d: &Dispatcher,
    ticket: Ticket,
    s: u32,
    f: Frame<I, O>,
    (class, deadline_ns): (DeadlineClass, u64),
    lent: Lent,
    watch: Duration,
) -> Done<I, O> {
    d.submit_watched(p, ticket, s, f, class, deadline_ns, lent, watch)
        .wait_done()
}

fn answered<I, O>(done: &Done<I, O>) -> Called {
    Called {
        outcome: done.outcome,
        error: done.error.clone(),
        lease: done.lease,
        recall: None,
    }
}

fn validate(p: &Plugin<Plane>, settings: &[u8]) -> Called {
    let mut f: Frame<ValidateIn, OutHead> = Frame::new(input(), output());
    f.input.settings = json(settings);
    p.call(life::VALIDATE, &mut f)
}
fn release(p: &Plugin<Plane>, lease: u64) -> Called {
    let mut f: Frame<ReleaseIn, OutHead> = Frame::new(input(), output());
    f.input.lease = lease;
    p.call(life::RELEASE, &mut f)
}
fn close(p: &Plugin<Plane>) -> Called {
    let mut f: Frame<InHead, OutHead> = Frame::new(input(), output());
    p.call(life::CLOSE, &mut f)
}
fn bind(d: &Dispatcher) -> Bind {
    Bind {
        instance: Arc::from("llm-concrete-cases"),
        max_inflight_cap: 64,
        sink: Arc::new(NoSink),
        dispatcher: d.adopter(),
        conns: ConnTable::Probe,
    }
}
fn plugin_cdylib() -> std::path::PathBuf {
    let path = std::path::PathBuf::from(
        std::env::var_os("BUSBAR_PLUGIN_CDYLIB")
            .expect("build the actual LLM cdylib and set BUSBAR_PLUGIN_CDYLIB"),
    );
    assert!(path.is_absolute(), "BUSBAR_PLUGIN_CDYLIB must be absolute");
    assert!(
        path.is_file(),
        "actual LLM cdylib missing: {}",
        path.display()
    );
    path
}
fn load(dropped: bool, d: &Dispatcher) -> Plugin<Plane> {
    let row = LinkedRow::of(busbar_plane_llm::door).expect("actual LLM statement");
    if dropped {
        load_dropped::<Plane>(&plugin_cdylib(), &row.statement, bind(d))
            .expect("actual dropped LLM loads")
    } else {
        load_linked::<Plane>(&row, bind(d)).expect("actual linked LLM loads")
    }
}
#[test]
fn all_llm_cases_match_through_the_linked_and_fresh_dropped_product() {
    assert_eq!(
        exercise(false),
        exercise(true),
        "LLM operation and billing transcripts through both doors"
    );
}

#[path = "support/door_refusals.rs"]
mod door_refusals;

#[test]
fn llm_declares_its_actual_statement_needs_and_requested_profile() {
    let declared: Value = serde_json::from_str(busbar_plane_llm::DECLARES).unwrap();
    let row = LinkedRow::of(busbar_plane_llm::door).unwrap();
    let statement = busbar_contract::abi::mechanism::rendering::read(&row.statement).unwrap();
    let mut needs: Vec<_> = statement
        .needs
        .iter()
        .map(|need| need.transport.as_str())
        .collect();
    needs.sort();
    needs.dedup();
    let mut declared: Vec<_> = declared["needs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|need| need.as_str().unwrap())
        .collect();
    declared.sort();
    declared.dedup();
    assert_eq!(needs, declared);
    if std::env::var_os("BUSBAR_EXPECT_RELEASE").is_some() {
        #[cfg(debug_assertions)]
        panic!("release profile requested");
    }
}
#[path = "support/ready_refusal.rs"]
mod ready_refusal;
