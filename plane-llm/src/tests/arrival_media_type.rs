//! Media-type tokens are case insensitive; form boundary values are not.
use crate::exchange::arrive::arrive;

#[test]
fn mixed_case_json_media_type_reads_model() {
    let fields = [(
        b"content-type".as_slice(),
        b"Application/JSON; charset=utf-8".as_slice(),
    )];
    let arrived = arrive(
        "POST",
        "/v1/chat/completions",
        &fields,
        br#"{"model":"gpt","messages":[]}"#,
        &(),
    )
    .unwrap();
    assert_eq!(arrived.model, "gpt");
}

#[test]
fn mixed_case_multipart_preserves_boundary_case() {
    let fields = [(
        b"content-type".as_slice(),
        b"Multipart/Form-Data; boundary=AbC".as_slice(),
    )];
    let body =
        b"--AbC\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n--AbC--\r\n";
    let arrived = arrive("POST", "/v1/audio/transcriptions", &fields, body, &()).unwrap();
    assert_eq!(arrived.model, "whisper-1");
}

#[test]
fn json_media_type_prefix_is_not_json() {
    let fields = [(b"content-type".as_slice(), b"application/jsonp".as_slice())];
    assert!(arrive(
        "POST",
        "/v1/chat/completions",
        &fields,
        br#"{"model":"gpt","messages":[]}"#,
        &()
    )
    .is_err());
}
