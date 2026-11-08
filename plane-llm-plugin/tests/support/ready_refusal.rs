// SPDX-License-Identifier: Apache-2.0
//! One malformed READY response over the actual LLM lifecycle.
use super::*;
use busbar_contract::abi::mechanism::call::RawOutcome;
use busbar_contract::abi::mechanism::door::{Door, DoorParts};
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
static READY_DOOR: AtomicPtr<Door> = AtomicPtr::new(std::ptr::null_mut());
static READY_CALLS: AtomicU64 = AtomicU64::new(0);
extern "C" fn ready_door() -> *const Door {
    READY_DOOR.load(Ordering::SeqCst)
}
extern "C" fn fail_ready(
    _: *mut std::ffi::c_void,
    _: *const std::ffi::c_void,
    out: *mut std::ffi::c_void,
) -> RawOutcome {
    READY_CALLS.fetch_add(1, Ordering::SeqCst);
    let failed = RawOutcome::of(Outcome::Failed);
    // SAFETY: READY lends its full OutHead; error text remains process-static.
    unsafe {
        let out = &mut *out.cast::<OutHead>();
        out.outcome = failed;
        out.error = AbiStr::of("LLM readiness refused");
    }
    failed
}
#[test]
fn a_failing_llm_ready_is_refused_once_with_the_plugins_text() {
    let original = busbar_plane_llm::door().parts();
    // SAFETY: all static product data retained; READY alone is replaced by a bounded raw negative.
    let door = unsafe {
        Door::__from_parts(DoorParts {
            size: std::mem::size_of::<Door>() as u32,
            ready: Some(fail_ready),
            ..original
        })
    };
    READY_DOOR.store(Box::into_raw(Box::new(door)), Ordering::SeqCst);
    let d = Arc::new(Dispatcher::new(
        DispatchConfig::default(),
        busbar_contract::services::test_caller_binding(),
    ));
    let row = LinkedRow::of_raw(ready_door).unwrap();
    let p = load_linked::<Plane>(&row, bind(&d)).unwrap();
    assert!(p.has_ready());
    let config: Value = serde_json::from_str(include_str!("../conformance.json")).unwrap();
    let settings = serde_json::to_vec(&config["settings"]).unwrap();
    let opened = open(&p, &handed(&p.served(), &settings), None);
    assert!(opened.starts_with("Ready "), "{opened}");
    let probe = p.test_invocation_probe();
    let first = || {
        let (crossings, resumes) = probe();
        crossings - resumes
    };
    let before = first();
    let refused = p
        .ready(&d, OP_DEADLINE)
        .expect_err("READY failure refuses boot");
    assert_eq!(
        refused,
        format!("plugin '{}' ready failed: LLM readiness refused", p.name())
    );
    assert_eq!(first() - before, 1, "READY is not retried");
    assert_eq!(READY_CALLS.load(Ordering::SeqCst), 1);
}
