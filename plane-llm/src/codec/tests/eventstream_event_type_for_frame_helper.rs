/// Resolve the event-type token `drain_frames_checked` returns for a frame: `:event-type` verbatim
/// for a normal event; for a modeled exception frame (`:message-type: exception`, `:exception-type`,
/// no `:event-type`) the name with the first letter lowercased (`InternalServerException` to
/// `internalServerException`) so it matches the reader's exception arms.
#[cfg(test)]
fn event_type_for_frame(headers: &[u8]) -> String {
    event_type_from_headers(&parse_frame_headers(headers))
}
