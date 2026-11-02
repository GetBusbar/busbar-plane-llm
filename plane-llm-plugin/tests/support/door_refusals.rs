// SPDX-License-Identifier: Apache-2.0
//! Malformed metadata over the actual LLM door; no substitute plane implementation.
use super::*;
use busbar_contract::abi::mechanism::call::AbiStr;
use busbar_contract::abi::mechanism::door::{Door, DoorParts, Statement};
use busbar_contract::abi::mechanism::{rendering, KindCode};
use busbar_plugin_loader::dispatch::load::rendering_of_raw;
use busbar_plugin_loader::dispatch::{Kind, LoadError};
use std::sync::atomic::{AtomicPtr, Ordering};

static MUTATED: AtomicPtr<Door> = AtomicPtr::new(std::ptr::null_mut());
extern "C" fn mutated() -> *const Door {
    MUTATED.load(Ordering::SeqCst)
}
extern "C" fn real() -> *const Door {
    busbar_plane_llm::plane_door::door()
}
// Used by one sequential test only. The real table and all newly allocated metadata remain static.
fn restate(parts: DoorParts) {
    // SAFETY: only ABI/layout numbers or a whole leaked Statement change. Every nested pointer
    // otherwise comes from the actual product's immutable static door.
    let door = unsafe { Door::__from_parts(parts) };
    MUTATED.store(Box::into_raw(Box::new(door)), Ordering::SeqCst);
}
#[test]
fn incompatible_metadata_is_refused_before_any_plane_slot_runs() {
    let lib = plugin_cdylib();
    let stated = rendering_of(busbar_plane_llm::plane_door::door).unwrap();
    let d = Arc::new(Dispatcher::new(DispatchConfig::default()));
    drop(load(false, &d));
    drop(load(true, &d));
    let d = Arc::new(Dispatcher::new(DispatchConfig::default()));
    let original = busbar_plane_llm::plane_door::door().parts();
    answer_layout_refusal(original, &d);
    let kind = KindCode::Plane;
    let abi = kind.abi_version();
    assert_eq!(original.kind_abi, abi);
    for version in [abi + 1, abi.wrapping_sub(1)] {
        restate(DoorParts {
            kind_abi: version,
            ..original
        });
        let expected = LoadError::KindAbi {
            kind,
            door: version,
            host: abi,
        };
        assert_eq!(rendering_of_raw(mutated).err(), Some(expected.clone()));
        let error = LinkedRow::of_raw(mutated)
            .and_then(|row| load_linked::<Plane>(&row, bind(&d)))
            .err()
            .unwrap();
        assert_eq!(error, expected);
        assert!(error.to_string().contains("rebuild"));
        let mut manifest = stated.clone();
        let at = rendering::RENDERING_MAGIC.len() + 8;
        manifest[at..at + 4].copy_from_slice(&version.to_le_bytes());
        assert_eq!(
            load_dropped::<Plane>(&lib, &manifest, bind(&d)).err(),
            Some(LoadError::ManifestKindAbi {
                stated: version,
                host: abi
            })
        );
    }
    let layout = kind.layout_fingerprint();
    assert_eq!(original.layout, layout);
    for fingerprint in [layout ^ 1, 0] {
        restate(DoorParts {
            layout: fingerprint,
            ..original
        });
        let error = rendering_of_raw(mutated).unwrap_err();
        let LoadError::KindLayout {
            kind: said,
            stated: written,
            host,
            ..
        } = &error
        else {
            panic!("{error:?}")
        };
        assert_eq!((*said, *written, *host), (kind, fingerprint, layout));
        assert!(error
            .to_string()
            .contains("rebuild against this busbar-contract"));
        let row = LinkedRow {
            statement: stated.clone(),
            door: mutated,
        };
        assert_eq!(
            load_linked::<Plane>(&row, bind(&d)).err(),
            Some(error.clone())
        );
        assert_eq!(
            LinkedRow::of_raw(mutated)
                .and_then(|row| load_linked::<Plane>(&row, bind(&d)))
                .err(),
            Some(error)
        );
    }
    // SAFETY: the actual door's validated Statement is immutable static product data.
    let statement = unsafe { original.statement.read_unaligned() };
    const VERSION: &str = "0.0.0-conformance-restated";
    let changed = Box::leak(Box::new(Statement {
        version: AbiStr {
            ptr: VERSION.as_ptr(),
            len: VERSION.len(),
        },
        ..statement
    }));
    restate(DoorParts {
        statement: changed,
        ..original
    });
    let lying = rendering_of_raw(mutated).unwrap();
    rendering::read(&lying).unwrap();
    assert_ne!(lying, stated);
    let row = LinkedRow {
        statement: lying.clone(),
        door: real,
    };
    assert_eq!(
        load_linked::<Plane>(&row, bind(&d)).err(),
        Some(LoadError::StatementMismatch)
    );
    assert_eq!(
        load_dropped::<Plane>(&lib, &lying, bind(&d)).err(),
        Some(LoadError::StatementMismatch)
    );
    let row = LinkedRow {
        statement: stated,
        door: mutated,
    };
    assert_eq!(
        load_linked::<Plane>(&row, bind(&d)).err(),
        Some(LoadError::StatementMismatch)
    );
}
fn wrong<K: Kind>() {
    let lib = plugin_cdylib();
    let stated = rendering_of(busbar_plane_llm::plane_door::door).unwrap();
    let row = LinkedRow::of(busbar_plane_llm::plane_door::door).unwrap();
    let d = Arc::new(Dispatcher::new(DispatchConfig::default()));
    let want = Some(LoadError::ManifestKind {
        stated: KindCode::Plane,
        want: K::CODE,
    });
    assert_eq!(load_linked::<K>(&row, bind(&d)).err(), want);
    assert_eq!(load_dropped::<K>(&lib, &stated, bind(&d)).err(), want);
}
#[test]
fn every_other_kind_refuses_the_actual_plane_in_both_deliveries() {
    use busbar_plugin_loader::dispatch::kinds::{
        auth::Auth, export::Export, hook::Hook, secret::Secret, store::Store, transport::Transport,
    };
    wrong::<Auth>();
    wrong::<Export>();
    wrong::<Hook>();
    wrong::<Store>();
    wrong::<Secret>();
    wrong::<Transport>();
}

fn answer_layout_refusal(original: DoorParts, d: &Dispatcher) {
    use busbar_contract::abi::mechanism::door::KindTailHead;
    use busbar_contract::abi::plane::{PlaneTail, ANSWER_LAYOUT};
    // SAFETY: reads only the actual validated plane tail's stated bytes, bounded by the host tail.
    let mut statement = unsafe { original.statement.read_unaligned() };
    let size = unsafe { statement.kind_tail.read_unaligned() }.size as usize;
    let mut tail: PlaneTail = unsafe { std::mem::zeroed() };
    unsafe {
        std::ptr::copy_nonoverlapping(
            statement.kind_tail.cast::<u8>(),
            std::ptr::from_mut(&mut tail).cast::<u8>(),
            size.min(std::mem::size_of::<PlaneTail>()),
        );
    }
    tail.answer_layout = ANSWER_LAYOUT - 1;
    statement.kind_tail = std::ptr::from_ref(Box::leak(Box::new(tail))).cast::<KindTailHead>();
    restate(DoorParts {
        statement: Box::leak(Box::new(statement)),
        ..original
    });
    let error = rendering_of_raw(mutated).expect_err("another answer layout cannot be packed");
    let LoadError::PlaneLayout { stated, host, .. } = &error else {
        panic!("{error:?}")
    };
    assert_eq!((*stated, *host), (ANSWER_LAYOUT - 1, ANSWER_LAYOUT));
    assert!(error
        .to_string()
        .contains("rebuild against this busbar-contract"));
    assert_eq!(
        LinkedRow::of_raw(mutated)
            .and_then(|row| load_linked::<Plane>(&row, bind(d)))
            .err(),
        Some(error)
    );
}

#[test]
fn llm_serving_requires_all_six_actual_http_needs_in_both_deliveries() {
    let row = LinkedRow::of(busbar_plane_llm::plane_door::door).unwrap();
    let statement = rendering::read(&row.statement).unwrap();
    assert_eq!(
        statement.needs.len(),
        6,
        "the actual LLM egress styles remain declared"
    );
    assert!(statement.needs.iter().all(|need| need.transport == "http"));
    let expected = LoadError::NoConnectionTable {
        plugin: statement.name.clone(),
        needs: 6,
    };
    let d = Arc::new(Dispatcher::new(DispatchConfig::default()));
    let serving = || Bind {
        conns: ConnTable::NoNeeds,
        ..bind(&d)
    };
    let linked = load_linked::<Plane>(&row, serving())
        .err()
        .expect("actual networked plane needs a table");
    assert_eq!(linked, expected);
    assert!(linked
        .to_string()
        .contains(&format!("plugin '{}'", statement.name)));
    assert_eq!(
        load_dropped::<Plane>(&plugin_cdylib(), &row.statement, serving()).err(),
        Some(expected)
    );
    load_linked::<Plane>(&row, bind(&d))
        .expect("a metadata Probe is admitted without serving connections");
}
