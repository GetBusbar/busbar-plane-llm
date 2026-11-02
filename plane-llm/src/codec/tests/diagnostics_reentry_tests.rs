use super::warn_error_map_class_unrecognized;
use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};

#[test]
fn an_error_map_warning_allows_synchronous_subscriber_reentry() {
    const VALUE: &str = "fixture_reentrant_unknown_error_map_class";
    struct Reentrant(Arc<AtomicUsize>);
    impl tracing::Subscriber for Reentrant {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool { true }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {
            self.0.fetch_add(1, Ordering::Relaxed);
            warn_error_map_class_unrecognized(VALUE);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }
    let count = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::with_default(Reentrant(count.clone()), || {
        // Exercise the actual caller, not only the dedupe helper.
        let mut map = std::collections::HashMap::new();
        map.insert("fixture".to_string(), VALUE.to_string());
        let mut raw = busbar_contract::upstream::RawUpstreamError::from_status(503);
        raw.provider_code = Some("fixture".to_string());
        let signal = crate::exchange::reply::failure::normalize(503, &raw, &map);
        assert_eq!(signal.class, busbar_contract::upstream::StatusClass::ServerError);
    });
    assert_eq!(count.load(Ordering::Relaxed), 1);
}
