// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Tests for `multipart_model` (moved with it from the legacy engine's ingress).

use busbar_plane_llm::exchange::multipart::multipart_model;

/// The `Content-Type` each body below arrives under. The boundary is not decoration: it is the only

const CT_BOUNDARY: &str = "multipart/form-data; boundary=BOUNDARY";
const CT_B: &str = "multipart/form-data; boundary=B";
const CT_ZZ: &str = "multipart/form-data; boundary=zz";

#[test]
fn extracts_model_before_large_binary_file() {
    let mut body = Vec::new();
    body.extend_from_slice(
        b"--BOUNDARY\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n",
    );
    body.extend_from_slice(
        b"--BOUNDARY\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a\"\r\n\r\n",
    );
    body.extend(std::iter::repeat_n(0u8, 1 << 20));
    body.extend_from_slice(b"\r\n--BOUNDARY--\r\n");
    assert_eq!(
        multipart_model(CT_BOUNDARY, &body).as_deref(),
        Some("whisper-1")
    );
}

#[test]
fn absent_model_is_none() {
    let body = b"--B\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nx\r\n--B--\r\n";
    assert_eq!(multipart_model(CT_B, body), None);
}

/// A `name="model"` SPELLED INSIDE ANOTHER PART'S VALUE IS NOT A PART HEADER.

#[test]
fn a_decoy_model_inside_another_parts_value_does_not_win() {
    let mut body = Vec::new();
    body.extend_from_slice(b"--zz\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\n");

    body.extend_from_slice(
        b"transcribe this\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ncheap-model",
    );
    body.extend_from_slice(b"\r\n--zz\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n");
    body.extend_from_slice(b"expensive-model\r\n");
    body.extend_from_slice(b"--zz\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\n");
    body.extend_from_slice(&[0u8, 1, 2]);
    body.extend_from_slice(b"\r\n--zz--\r\n");

    assert_eq!(
        multipart_model(CT_ZZ, &body).as_deref(),
        Some("expensive-model"),
        "busbar must verify, admit and bill the model the provider will actually serve"
    );
}

/// TWO `model` PARTS IS NOT A REQUEST WITH A MODEL.

#[test]
fn two_model_parts_resolve_to_no_model() {
    let body = concat!(
        "--zz\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nfirst\r\n",
        "--zz\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nsecond\r\n",
        "--zz--\r\n"
    );
    assert_eq!(multipart_model(CT_ZZ, body.as_bytes()), None);
}

fn large_file_form(early_model: bool, late_model: bool) -> Vec<u8> {
    let mut body = Vec::new();
    if early_model {
        body.extend_from_slice(
            b"--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ncheap-model\r\n",
        );
    }
    body.extend_from_slice(
        b"--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"model\"\r\n\r\n",
    );
    body.extend(std::iter::repeat_n(0xff, 1 << 20));
    body.extend_from_slice(b"\r\n");
    if late_model {
        body.extend_from_slice(
            b"--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nexpensive-model\r\n",
        );
    }
    body.extend_from_slice(b"--B--\r\n");
    body
}

#[test]
fn arrival_refuses_a_duplicate_model_after_a_large_file() {
    use busbar_plane_llm::exchange::arrive::{arrive, Decline};

    let body = large_file_form(true, true);
    let headers = [(b"content-type".as_slice(), CT_B.as_bytes())];
    let refusal = arrive("POST", "/v1/audio/transcriptions", &headers, &body, &())
        .expect_err("late duplicate model must not admit the earlier cheap model");
    assert_eq!((refusal.why, refusal.status), (Decline::MissingModel, 400));
}

#[test]
fn arrival_accepts_a_single_model_on_either_side_of_a_large_file() {
    use busbar_plane_llm::exchange::arrive::arrive;

    let headers = [(b"content-type".as_slice(), CT_B.as_bytes())];
    for (early, late, expected) in [
        (true, false, "cheap-model"),
        (false, true, "expensive-model"),
    ] {
        let body = large_file_form(early, late);
        let arrival = arrive("POST", "/v1/audio/transcriptions", &headers, &body, &())
            .expect("one model and a large file are a valid upload");
        assert_eq!(arrival.model, expected);
        assert_eq!(arrival.body, body);
    }
}

#[test]
fn delimiter_lookalikes_in_a_file_do_not_hide_the_model() {
    let body = concat!(
        "--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"model\"\r\n\r\n",
        "bytes--B\r\n--B--suffix\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ndecoy\r\n",
        "--B \t\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n",
        "--B-- \t\r\nepilogue"
    );
    assert_eq!(
        multipart_model(CT_B, body.as_bytes()).as_deref(),
        Some("whisper-1")
    );
}

#[test]
fn disposition_parameters_are_whole_keys_outside_quoted_filenames() {
    let body = concat!(
        "--B\r\nContent-Disposition: form-data; filename=\"model\"; name=\"file\"\r\n\r\naudio\r\n",
        "--B\r\nX-Note: name=\"decoy\"\r\n",
        "content-disposition: form-data; filename=\"a;\\\"b\"; NAME = \"mo\\del\"\r\n\r\nwhisper-1\r\n",
        "--B--\r\n"
    );
    assert_eq!(
        multipart_model(CT_B, body.as_bytes()).as_deref(),
        Some("whisper-1")
    );
}

#[test]
fn an_ambiguous_part_name_cannot_hide_a_second_model() {
    use busbar_plane_llm::exchange::arrive::{arrive, Decline};

    let body = concat!(
        "--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ncheap-model\r\n",
        "--B\r\nContent-Disposition: form-data; name=\"file\"; name=\"model\"\r\n\r\nexpensive-model\r\n",
        "--B--\r\n"
    );
    let headers = [(b"content-type".as_slice(), CT_B.as_bytes())];
    let refusal = arrive(
        "POST",
        "/v1/audio/transcriptions",
        &headers,
        body.as_bytes(),
        &(),
    )
    .expect_err("an ambiguous field cannot be skipped when determining the admitted model");
    assert_eq!((refusal.why, refusal.status), (Decline::MissingModel, 400));
}
