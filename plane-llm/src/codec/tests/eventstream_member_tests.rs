//! CONFORMANCE (botocore `bedrock-runtime/2023-09-30/service-2.json`, `ConverseStreamOutput`): the
//! `:exception-type` of a modeled exception frame names a MEMBER of the output union, and the
//! members are lowerCamel (`internalServerException`). v1.5.5 sent the PascalCase shape name, which
//! is not a member, so a decoder that looks the header up among the union's members finds none.
use super::*;

fn exception_type_of(frame: &[u8]) -> String {
    let headers_len = u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]) as usize;
    let headers = &frame[PRELUDE_LEN..PRELUDE_LEN + headers_len];
    let name = b":exception-type";
    let at = headers
        .windows(name.len())
        .position(|w| w == name)
        .expect("the :exception-type header is present");
    // header layout: u8 name_len, name, u8 type(7), u16 value_len, value
    let v = at + name.len() + 1;
    let vlen = u16::from_be_bytes([headers[v], headers[v + 1]]) as usize;
    String::from_utf8(headers[v + 2..v + 2 + vlen].to_vec()).unwrap()
}

#[test]
fn the_wire_carries_the_union_member_not_the_shape_name() {
    for (given, member) in [
        ("InternalServerException", "internalServerException"),
        ("ModelStreamErrorException", "modelStreamErrorException"),
        ("internalServerException", "internalServerException"),
    ] {
        assert_eq!(
            exception_type_of(&encode_exception_frame(given, "x")),
            member,
            "given {given}"
        );
    }
}
