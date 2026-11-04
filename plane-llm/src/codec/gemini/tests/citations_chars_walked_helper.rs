#[cfg(test)]
thread_local! {
    pub(super) static CHARS_WALKED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
