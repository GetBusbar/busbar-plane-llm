// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

async fn actual_listener_with_routers(
    app: Arc<busbar_kernel::state::App>,
    config: &busbar_kernel::config::RootCfg,
    book: &crate::root::durability::NodeBook,
    with_llm: bool,
) -> (kin::Listener, axum::Router, axum::Router) {
    use crate::root::loader::dispatch::{
        kinds::plane::Plane, load_linked, Bind, ConnTable, LinkedRow, NoSink,
    };
    use crate::root::serve::{LateServices, ListenerPlan};
    let late = LateServices::new();
    let (credentials, _handle) = crate::root::credentials::AppCredentials::late();
    crate::root::serve::compose(
        crate::root::connector::dest_judge(config),
        &late,
        credentials,
    );
    let declarations: Vec<_> = busbar_kernel::plane_driver::registry::plane_decls()
        .iter()
        .map(|decl| &decl.declaration)
        .collect();
    crate::root::serve::attach(
        &late,
        app.governance.as_deref(),
        None,
        &app.demotion_record,
        &declarations,
    );
    let dispatcher = Arc::new(crate::root::dispatch::build(2, &late));
    crate::root::connector::install_io(&dispatcher);
    let connector = crate::root::connector::the().clone();
    let row = LinkedRow::of(busbar_plane_llm::door)
        .expect("the normal linked LLM product states its actual memory ABI");
    let plugin = Arc::new(
        load_linked::<Plane>(
            &row,
            Bind {
                instance: "inbound-actual-llm".into(),
                max_inflight_cap: 64,
                sink: Arc::new(NoSink),
                dispatcher: dispatcher.adopter(),
                conns: ConnTable::Host(crate::root::connector::admit(connector.clone())),
            },
        )
        .expect("the actual linked LLM image binds to normal host services"),
    );
    let section = plugin.served().section;
    let mut sections = std::collections::BTreeMap::new();
    if with_llm {
        sections = app
            .config_projection
            .as_ref()
            .and_then(|projection| projection.downcast_ref::<crate::root::door_steps::DoorConfig>())
            .expect("the normal root retains the actual resolved plane-owned sections")
            .sections
            .clone()
            .expect("the validated root projection contains valid plane-owned sections");
        assert!(
            sections.contains_key(section),
            "the actual fallback section is configured"
        );
    }
    let gov = app
        .governance
        .as_ref()
        .expect("the actual Store owns governance")
        .clone();
    let node = Arc::new(crate::root::plane_node::Node::new());
    node.bind_book(Arc::clone(&book.durability));
    let post = Arc::new(crate::root::plane_node::NodeEndPost::new(node));
    let money = || {
        Arc::new(busbar_kernel::plane_driver::PlaneMoney::new(
            gov.clone(),
            post.clone(),
        ))
    };
    let served = crate::root::serve::compose_planes(
        &[("inbound-actual-llm".into(), plugin)],
        &dispatcher,
        &late,
        &sections,
        Some("https://gw.example"),
        &money,
        None,
        None,
        &Default::default(),
    )
    .await
    .expect("the actual linked LLM OPEN claims are admitted by the normal driver");
    let framer = move |scheme: &str| connector.framer_for(scheme);
    let topology = crate::root::boot::configured_listener_needs(config, &sections)
        .expect("the actual configuration declares its normal physical listener needs");
    let plan = Arc::new(ListenerPlan::new(
        topology,
        crate::root::serve::transport_offers(crate::LINKED.transports, &[]),
        &framer,
    ));
    plan.mount_planes(&served, &app, &[])
        .expect("the actual LLM claims prepare the normal listener and refusal renderer");
    let source = kin::Listener {
        guests: GuestList::default(),
        auths: Default::default(),
        body_limit: 16,
        app,
        door_refusals: Vec::new(),
        ids: Vec::new(),
        signed_ingress: Arc::new(|_, _| false),
        scope: None,
    };
    let mut listener = plan.policy("listen".into())(&source)
        .expect("the actual prepared listener retains its generation's producer");
    if with_llm {
        let plane = served
            .planes
            .iter()
            .find(|plane| plane.instance == "inbound-actual-llm")
            .expect("the actual LLM instance owns this listener's producer");
        let (rung, claim) = plane
            .snapshot
            .claims
            .iter()
            .enumerate()
            .find(|(_, claim)| {
                claim.verb == "POST"
                    && claim.target == "/"
                    && claim.flags & busbar_contract::abi::plane::CLAIM_EXACT == 0
            })
            .expect("the actual OPEN snapshot declares its non-exact POST root fallback");
        let rung = u32::try_from(rung).expect("the actual claim index fits the host route field");
        let mounted = listener.guests.lines().iter().find(|line| {
            matches!(&line.claimant, busbar_kernel::guest::Claimant::Plane(owner) if owner == &plane.instance)
                && line.route.rung == rung
        }).expect("the normal listener plan mounts that exact actual claim");
        assert_eq!(
            mounted.dialect,
            u32::from(claim.refusal_dialect),
            "the mounted line retains the actual claim's opaque refusal dialect"
        );
        assert!(
            matches!(listener.guests.matched("POST", "/definitely/unknown", &[]),
            Matched::Line(line) if std::ptr::eq(line, mounted)),
            "the captured unknown path reaches the actual mounted fallback claim"
        );
    }
    listener.ids = listener
        .guests
        .lines()
        .iter()
        .map(|_| auth_handle::next_ref_id(RefSpace::Line))
        .collect();
    let handle = Arc::new(busbar_kernel::state::AppHandle::new(Arc::clone(
        &source.app,
    )));
    let select = Arc::clone(&plan);
    let data = busbar_kernel::router::serve_listener_generations(
        Arc::clone(&handle),
        move |app| select.prepared(app, "listen"),
        32 * 1024 * 1024,
        0,
        false,
    );
    let admin = busbar_kernel::router::build_admin_router_on(&handle, 32 * 1024 * 1024, false);
    (listener, data, admin)
}
