impl DrainWork {
    /// Every byte the pass touched: walked as frames plus moved by an edit.
    #[cfg(test)]
    pub(crate) fn total(&self) -> usize {
        self.scanned + self.shifted
    }
}
