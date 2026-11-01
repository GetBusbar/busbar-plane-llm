// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Tests for `multipart_model` (moved with it from the legacy engine's ingress).

use busbar_plane_llm::exchange::multipart::multipart_model;

/// The `Content-Type` each body below arrives under. The boundary is not decoration: it is the only
/// thing that says where a part begins, so every case names the one its bytes were written with.
const CT_BOUNDARY: &str = "multipart/form-data; boundary=BOUNDARY";
const CT_B: &str = "multipart/form-data; boundary=B";
const CT_ZZ: &str = "multipart/form-data; boundary=zz";

#[test]
fn extracts_model_before_large_binary_file() {
    // A well-formed transcription: the `model` text part precedes a large binary audio part.
    // A large file must remain accepted while the complete form is checked for duplicate models.
    let mut body = Vec::new();
    body.extend_from_slice(
        b"--BOUNDARY\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n",
    );
    body.extend_from_slice(
        b"--BOUNDARY\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a\"\r\n\r\n",
    );
    body.extend(std::iter::repeat_n(0u8, 1 << 20)); // 1 MiB of binary, not valid UTF-8
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
///
/// This is the whole of the multipart read's security argument. The model decides which lane the
/// unit is verified against, which pool it is admitted to and which rate card it is billed on; the
/// provider decides which model actually answers, by parsing the same body with a real multipart
/// parser. If those two readers can be made to disagree, the caller picks what busbar charges for
/// independently of what it receives.
///
/// A raw scan for the first `name="model"` in the body can be made to disagree trivially. The
/// request below is well-formed: a `prompt` text part, then a real `model` part naming an expensive
/// model. Every conforming parser reads the model as `expensive-model`, because the only `model`
/// PART is the second one — the earlier occurrence is characters inside the prompt's value, which is
/// user text and is not structure. A scanner that never looks at the boundary reads the decoy,
/// because the decoy comes first.
///
/// The assertion is the identity, not the string: whatever busbar bills is what the provider
/// serves.
#[test]
fn a_decoy_model_inside_another_parts_value_does_not_win() {
    let mut body = Vec::new();
    body.extend_from_slice(b"--zz\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\n");
    // The caller's own prompt text. It is a VALUE: a multipart parser copies these bytes out
    // verbatim and never reads a header out of them.
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
///
/// A conforming multipart body may repeat a field name, and there is no rule that says which
/// repetition a provider takes — some take the first, some the last. So a body carrying two is a
/// body whose model busbar cannot know, and guessing at one is guessing at what will be billed.
/// It resolves to no model at all, which the ladder's floor turns into the same missing-model
/// refusal an absent field gets: a refusal before the door costs the caller nothing and cannot be
/// made to charge for the wrong lane.
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
