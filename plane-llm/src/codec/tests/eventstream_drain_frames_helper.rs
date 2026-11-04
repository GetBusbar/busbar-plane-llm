/// Drain every complete frame from `buf`, returning `(event_type, payload_bytes)` per frame and
/// leaving a trailing partial frame buffered.
#[cfg(test)]
pub fn drain_frames(buf: &mut Vec<u8>) -> Vec<(String, Vec<u8>)> {
    drain_frames_checked(buf, None).0
}
