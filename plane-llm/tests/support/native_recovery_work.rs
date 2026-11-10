use super::*;
use busbar_contract::abi::host::service::{
    HostSlots, ServiceOut, WorkFindIn, WorkOpenIn, WorkSettleIn, SERVICES,
};
use busbar_contract::abi::mechanism::{
    call::{RawOutcome, Span},
    ticket::{HostCtx, HostTables},
};
use std::{cell::RefCell, ffi::c_void};

#[derive(Default)]
struct State {
    record: Vec<u8>,
    opens: usize,
    settles: usize,
    pending: bool,
}
thread_local! { static STATE: RefCell<State> = RefCell::new(State::default()); }
const TICKET: Ticket = Ticket {
    slot: 42,
    generation: 1,
};
const REFERENCE: &[u8] = b"0123456789abcdef0123456789abcdef";
fn finish(out: *mut ServiceOut, outcome: Outcome, len: usize, items: u64) -> RawOutcome {
    // SAFETY: SDK supplies one writable output for the synchronous service call.
    unsafe {
        out.write(ServiceOut {
            size: std::mem::size_of::<ServiceOut>() as u32,
            outcome: RawOutcome::of(outcome),
            value: if items == 0 { 0 } else { 7 },
            len: len as u64,
            items,
            ..ServiceOut::ZERO
        });
    }
    RawOutcome::of(outcome)
}
extern "C" fn open(_: HostCtx, input: *const c_void, out: *mut ServiceOut) -> RawOutcome {
    // SAFETY: these fixtures are called only by the typed SDK with live loaned records/buffers.
    let input = unsafe { input.cast::<WorkOpenIn>().read_unaligned() };
    if STATE.with(|s| s.borrow().pending) {
        return finish(out, Outcome::Pending, 0, 0);
    }
    let record = unsafe { std::slice::from_raw_parts(input.record.ptr, input.record.len) };
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.record = record.to_vec();
        s.opens += 1;
    });
    unsafe {
        std::ptr::copy_nonoverlapping(REFERENCE.as_ptr(), input.into.buf, REFERENCE.len());
        input.into.spans.write(ItemSpan {
            key: Span {
                offset: 0,
                len: REFERENCE.len() as u32,
            },
            value: Span {
                offset: SPAN_ABSENT,
                len: 0,
            },
        });
    }
    finish(out, Outcome::Ready, REFERENCE.len(), 1)
}
extern "C" fn find(_: HostCtx, input: *const c_void, out: *mut ServiceOut) -> RawOutcome {
    let input = unsafe { input.cast::<WorkFindIn>().read_unaligned() };
    let record = STATE.with(|s| s.borrow().record.clone());
    unsafe {
        input.into.buf.write(WORK_LIVE);
        std::ptr::copy_nonoverlapping(record.as_ptr(), input.into.buf.add(1), record.len());
        input.into.spans.write(ItemSpan {
            key: Span { offset: 0, len: 1 },
            value: Span {
                offset: 1,
                len: record.len() as u32,
            },
        });
    }
    finish(out, Outcome::Ready, record.len() + 1, 1)
}
extern "C" fn settle(_: HostCtx, input: *const c_void, out: *mut ServiceOut) -> RawOutcome {
    let input = unsafe { input.cast::<WorkSettleIn>().read_unaligned() };
    assert_eq!(input.handle, 7);
    if STATE.with(|s| s.borrow().pending) {
        return finish(out, Outcome::Pending, 0, 0);
    }
    let record = unsafe { std::slice::from_raw_parts(input.record.ptr, input.record.len) };
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        assert_eq!(s.record, record);
        s.settles += 1;
    });
    finish(out, Outcome::Ready, 0, 0)
}
fn services() -> Services {
    let mut slots: HostSlots = unsafe { std::mem::zeroed() };
    slots.size = std::mem::size_of::<HostSlots>() as u32;
    slots.slots = SERVICES;
    slots.work_open = Some(open);
    slots.work_find = Some(find);
    slots.work_settle = Some(settle);
    let tables = HostTables {
        size: std::mem::size_of::<HostTables>() as u32,
        _reserved: 0,
        ctx: HostCtx { handle: 7 },
        wake: None,
        conns: std::ptr::null(),
        services: &slots,
        io: std::ptr::null(),
    };
    // SAFETY: Services copies the table, and all callbacks remain static through the tests.
    unsafe { Services::test_from_raw(&tables).unwrap() }
}

#[test]
fn recovery_work_survives_pending_open_and_settles_only_after_confirmation() {
    STATE.with(|s| {
        *s.borrow_mut() = State {
            pending: true,
            ..Default::default()
        }
    });
    let services = services();
    let mut work = Work::new(&"a".repeat(64));
    assert!(work.attach(&services, TICKET).is_pending());
    assert!(work.handle.is_none());
    STATE.with(|s| s.borrow_mut().pending = false);
    assert!(matches!(
        work.attach(&services, TICKET),
        Poll::Ready(Ok(()))
    ));
    assert!(matches!(
        work.attach(&services, TICKET),
        Poll::Ready(Ok(()))
    ));
    STATE.with(|s| assert_eq!(s.borrow().opens, 1));
    let fields = [(WORK_HEADER, REFERENCE)];
    let mut restored = Work::restore(&"a".repeat(64), &fields).unwrap();
    assert!(matches!(
        restored.attach(&services, TICKET),
        Poll::Ready(Ok(()))
    ));
    STATE.with(|s| s.borrow_mut().pending = true);
    assert!(restored.settle(&services, TICKET).is_pending());
    assert!(!restored.settled);
    STATE.with(|s| s.borrow_mut().pending = false);
    assert!(matches!(
        restored.settle(&services, TICKET),
        Poll::Ready(Ok(()))
    ));
    assert!(matches!(
        restored.settle(&services, TICKET),
        Poll::Ready(Ok(()))
    ));
    STATE.with(|s| assert_eq!(s.borrow().settles, 1));
}

#[test]
fn recovery_reference_cannot_redirect_to_another_intent_or_leak_upstream() {
    STATE.with(|s| {
        *s.borrow_mut() = State {
            record: poll_record(&target(&"b".repeat(64))).unwrap(),
            ..Default::default()
        }
    });
    let fields = [
        (WORK_HEADER, REFERENCE),
        (b"safe".as_slice(), b"value".as_slice()),
    ];
    let mut work = Work::restore(&"a".repeat(64), &fields).unwrap();
    assert!(matches!(
        work.attach(&services(), TICKET),
        Poll::Ready(Err(_))
    ));
    assert_eq!(
        forwarded(&fields),
        vec![(b"safe".to_vec(), b"value".to_vec())]
    );
    assert!(Work::restore(&"a".repeat(64), &[]).is_err());
    assert!(Work::restore(
        &"a".repeat(64),
        &[(WORK_HEADER, REFERENCE), (WORK_HEADER, REFERENCE)]
    )
    .is_err());
    assert!(intent("POST", &target(&"a".repeat(64))).is_err());
    assert!(intent("GET", &format!("{}?x=1", target(&"a".repeat(64)))).is_err());
}
