// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use crate::root::serve::planes_tests::PUBLISHING;
use crate::root::serve::secret_source_tests as scan;
use axum::http::{HeaderMap, StatusCode};
use busbar_contract::secret::SecretAxis;
use busbar_contract::secret_ref::SecretRef;

struct SourceFixture {
    reference: SecretRef,
    needles: Vec<String>,
}
impl SourceFixture {
    fn new(role: &str, value: &str) -> Self {
        #[cfg(distribution_root)]
        crate::dist_root_install::install();
        let reference = crate::root::linked::secret_rows()
            .sugars()
            .reference_for_role(role, value)
            .expect("the actual linked source declares the fixture role");
        let mut needles = scan::source_needles();
        needles.extend([value.to_owned(), reference.describe()]);
        Self { reference, needles }
    }
}
fn authored(secret: &SourceFixture) -> serde_json::Value {
    serde_json::to_value(
        secret
            .reference
            .authoring(&busbar_contract::caps::KernelSeal::acquire_for_kernel()),
    )
    .expect("the authored source reference")
}
fn source_in(render: &str, secret: &SourceFixture) -> Vec<String> {
    scan::source_in(render, &secret.needles)
}
fn assert_clean(
    case: &str,
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
    secret: &SourceFixture,
) {
    scan::assert_clean(case, status, headers, body, &secret.needles);
}
fn refused_naming_its_source(secret: &SourceFixture) -> String {
    #[cfg(distribution_root)]
    crate::dist_root_install::install();
    let text = busbar_kernel::config::secret::resolve_linked_string_at_config(
        &secret.reference,
        &busbar_contract::config_time::ConfigScope::config_time(),
    )
    .expect_err("the actual source refuses the reference");
    assert!(
        !source_in(&text, secret).is_empty(),
        "the actual source refusal must make the caller scan meaningful: {text}"
    );
    text
}
fn missing(case: &str) -> String {
    std::env::temp_dir()
        .join(format!(
            "busbar-secret-source-{case}-{}",
            std::process::id()
        ))
        .display()
        .to_string()
}
fn file(path: impl AsRef<str>) -> SourceFixture {
    SourceFixture::new(
        busbar_contract::abi::mechanism::door::SUGAR_ROLE_PATH,
        path.as_ref(),
    )
}
#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pools_door_an_unresolved_provider_key_refuses_the_boot_naming_its_source() {
    use crate::root::serve::plane_llm_door::hook_seat_tests::{
        far_end_answering, try_rig, RigOpts, ANSWER,
    };
    let _one = PUBLISHING.lock().await;
    let instance = "pools-key-compose";
    let path = missing(instance);
    let secret = file(path.clone());
    refused_naming_its_source(&secret);
    let far = far_end_answering(200, ANSWER).await;
    let api_key = authored(&secret).to_string();
    let Err(refusal) = try_rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            api_key: Some(&api_key),
            ..RigOpts::default()
        },
    )
    .await
    else {
        panic!("a provider key that does not resolve refuses the composition");
    };
    println!("SECRET-SOURCE {instance}: compose refused (operator): {refusal}");
    assert!(
        !source_in(&refusal, &secret).is_empty(),
        "the operator's refusal names the source: {refusal}"
    );
    assert_eq!(
        far.hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "nothing reached the far end"
    );
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pools_door_a_provider_key_gone_by_an_apply_reaches_the_caller_without_its_source() {
    use crate::root::serve::plane_llm_door::hook_seat_tests::{
        far_end_answering, rig, RigOpts, ANSWER,
    };
    let _one = PUBLISHING.lock().await;
    let instance = "pools-key-apply";
    let path = missing(instance);
    std::fs::write(&path, "sk-secret-source-test").expect("the credential file");
    let secret = file(path.clone());
    let far = far_end_answering(200, ANSWER).await;
    let api_key = authored(&secret).to_string();
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            api_key: Some(&api_key),
            ..RigOpts::default()
        },
    )
    .await;
    let (status, _, body) = rig.chat().await;
    assert_eq!(
        status,
        200,
        "served while it resolves: {}",
        String::from_utf8_lossy(&body)
    );
    std::fs::remove_file(&path).expect("the credential file goes");
    refused_naming_its_source(&secret);
    let before = rig.generation();
    rig.apply();
    assert_eq!(
        rig.generation(),
        before,
        "the door keeps the generation it serves"
    );
    let (status, headers, body) = rig.chat().await;
    let status = StatusCode::from_u16(status).expect("a status");
    assert_clean(instance, status, &headers, &body, &secret);
    assert_eq!(status, StatusCode::OK, "the kept generation serves");
}
