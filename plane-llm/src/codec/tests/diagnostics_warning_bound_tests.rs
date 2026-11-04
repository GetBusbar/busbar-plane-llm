use super::{fresh_error_map_warning, ERROR_MAP_WARNING_CACHE_CAPACITY};
use std::collections::VecDeque;

#[test]
fn warning_cache_bounds_retention_and_rewarns_after_fifo_eviction() {
    let mut seen = VecDeque::new();
    assert!(fresh_error_map_warning(&mut seen, "oldest"));
    for index in 1..ERROR_MAP_WARNING_CACHE_CAPACITY {
        assert!(fresh_error_map_warning(
            &mut seen,
            &format!("class-{index}")
        ));
    }
    assert!(!fresh_error_map_warning(&mut seen, "oldest"));
    assert!(fresh_error_map_warning(&mut seen, "overflow"));
    assert_eq!(seen.len(), ERROR_MAP_WARNING_CACHE_CAPACITY);
    assert!(!fresh_error_map_warning(&mut seen, "class-1"));
    assert!(fresh_error_map_warning(&mut seen, "oldest"));
    assert_eq!(seen.len(), ERROR_MAP_WARNING_CACHE_CAPACITY);
    for index in 0..10_000 {
        assert!(fresh_error_map_warning(
            &mut seen,
            &format!("reload-{index}")
        ));
        assert_eq!(seen.len(), ERROR_MAP_WARNING_CACHE_CAPACITY);
    }
}

#[test]
fn long_class_names_retain_only_a_fingerprint_and_still_deduplicate() {
    let mut seen = VecDeque::new();
    let long_class = "invalid".repeat(100_000);
    assert!(fresh_error_map_warning(&mut seen, &long_class));
    assert!(!fresh_error_map_warning(&mut seen, &long_class));
    assert_eq!(seen.len(), 1);
    assert_eq!(std::mem::size_of_val(&seen[0]), 8);
    assert!(fresh_error_map_warning(
        &mut seen,
        &(long_class + "different")
    ));
    assert_eq!(seen.len(), 2);
}
