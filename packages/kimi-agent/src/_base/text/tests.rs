use super::code_points::{code_point_boundary_start, slice_on_code_point_boundary};
use super::encoding::{
    ENCODING_DETECTION_SAMPLE_BYTES, UtfTextEncoding, classify_text_sample, decode_utf_text,
    detect_text_encoding,
};
use super::frontmatter::{FrontmatterError, parse_frontmatter};
use super::line_endings::split_lines_keeping_terminator;
use serde_json::json;

#[test]
fn core_version_is_not_empty() {
    assert!(!crate::_base::version::get_core_version().is_empty());
}

#[test]
fn code_point_slices_never_split_pairs() {
    let text = "aaa😀😁";
    assert_eq!(slice_on_code_point_boundary(text, 4), "aaa");
    assert_eq!(slice_on_code_point_boundary(text, 5), "aaa😀");
    assert_eq!(slice_on_code_point_boundary("abcdef", 3), "abc");
    assert_eq!(slice_on_code_point_boundary("short", 10), "short");
}

#[test]
fn code_point_start_advances_only_between_pair_halves() {
    assert_eq!(code_point_boundary_start("aaa😀zz", 4), 5);
    assert_eq!(code_point_boundary_start("abcdef", 3), 3);
    assert_eq!(code_point_boundary_start("abcdef", 0), 0);
    assert_eq!(code_point_boundary_start("abcdef", 6), 6);
}

#[test]
fn detects_all_boms_and_utf16_parity() {
    for (sample, expected) in [
        (&[0xef, 0xbb, 0xbf, 0x61][..], UtfTextEncoding::Utf8),
        (&[0xff, 0xfe, 0x61, 0x00][..], UtfTextEncoding::Utf16Le),
        (&[0xfe, 0xff, 0x00, 0x61][..], UtfTextEncoding::Utf16Be),
        (&[0x61, 0x00, 0x62, 0x00][..], UtfTextEncoding::Utf16Le),
        (&[0x00, 0x61, 0x00, 0x62][..], UtfTextEncoding::Utf16Be),
    ] {
        let detection = detect_text_encoding(sample);
        assert_eq!(detection.encoding, expected);
        assert!(!detection.seems_binary);
    }
}

#[test]
fn classification_covers_binary_and_text_paths() {
    assert!(!classify_text_sample(b"").is_binary);
    assert!(!classify_text_sample("日志 🚀\n".as_bytes()).is_binary);
    assert!(classify_text_sample(b"abc\0def").is_binary);
    assert!(classify_text_sample(&[0xd6, 0xd0, 0xc4, 0xe3, 0x31, 0x32]).is_binary);
    let controls = [&[0x1b; 40][..], &[0x61; 60][..]].concat();
    assert!(classify_text_sample(&controls).is_binary);
    let mut outside = vec![0x61; ENCODING_DETECTION_SAMPLE_BYTES + 2];
    outside[ENCODING_DETECTION_SAMPLE_BYTES + 1] = 0;
    assert!(classify_text_sample(&outside).is_binary);
}

#[test]
fn classification_tolerates_only_valid_truncated_utf8_prefixes() {
    let sample = ["日志记录\n".as_bytes(), &[0xe4, 0xb8][..]].concat();
    assert!(!classify_text_sample(&sample).is_binary);
    assert!(classify_text_sample(b"plain\n\xff").is_binary);
    assert!(classify_text_sample(b"plain\n\xe4\x41").is_binary);
}

#[test]
fn utf_decode_strips_bom_and_replaces_malformed_sequences() {
    assert_eq!(
        decode_utf_text(&[0xef, 0xbb, 0xbf, b'x'], UtfTextEncoding::Utf8),
        "x"
    );
    assert_eq!(
        decode_utf_text(&[0xff, 0xfe, 0x2d, 0x4e], UtfTextEncoding::Utf16Le),
        "中"
    );
    assert_eq!(
        decode_utf_text(&[0xfe, 0xff, 0x4e, 0x2d], UtfTextEncoding::Utf16Be),
        "中"
    );
    assert_eq!(decode_utf_text(&[0xff], UtfTextEncoding::Utf16Le), "�");
}

#[test]
fn frontmatter_supports_null_mapping_scalar_and_sequence_data() {
    assert_eq!(parse_frontmatter("just body").unwrap().data, json!(null));
    let parsed = parse_frontmatter("---\nname: foo\n---\nbody text").unwrap();
    assert_eq!(parsed.data, json!({"name":"foo"}));
    assert_eq!(parsed.body, "body text");
    assert_eq!(parse_frontmatter("---\n---\nbody").unwrap().data, json!({}));
    assert_eq!(
        parse_frontmatter("---\n- a\n- b\n---\nbody").unwrap().data,
        json!(["a", "b"])
    );
    assert_eq!(
        parse_frontmatter("---\nhello\n---\nbody").unwrap().data,
        json!("hello")
    );
}

#[test]
fn frontmatter_preserves_body_newlines_and_typed_errors() {
    let parsed = parse_frontmatter("---\r\na: b\r\n---\r\nbody\r\n").unwrap();
    assert_eq!(parsed.body, "body\n");
    let error: FrontmatterError = parse_frontmatter("---\nname: foo").unwrap_err();
    assert_eq!(error.name(), "FrontmatterError");
    assert_eq!(error.to_string(), "Missing closing frontmatter fence");
}

#[test]
fn lines_keep_terminators_and_unterminated_tail() {
    assert_eq!(split_lines_keeping_terminator("a\nb\n"), ["a\n", "b\n"]);
    assert_eq!(split_lines_keeping_terminator("a\nb"), ["a\n", "b"]);
    assert!(split_lines_keeping_terminator("").is_empty());
    assert_eq!(split_lines_keeping_terminator("\n"), ["\n"]);
}
