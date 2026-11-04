#[cfg(test)]
thread_local! {
    static DOCUMENT_PREPARATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
