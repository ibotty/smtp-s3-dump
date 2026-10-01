//! Extraction of e-mail attachments that preserves the original bytes, except for
//! CRLF line breaks in text parts.
//!
//! `mail-parser` classifies any `Content-Type: text/*` MIME part as `PartType::Text`,
//! decoding it into a UTF-8 `String` while parsing (see `mail_parser::parsers::message`).
//! That's correct for message bodies, but wrong for attachments such as CSV files:
//!
//! - quoted-printable decoding yields CRLF line breaks, even when the original
//!   attachment only used LF (`attachment_bytes` converts them back to LF),
//! - a missing/unrecognized `charset` falls back to `String::from_utf8_lossy`,
//!   replacing every non-UTF-8 byte (e.g. Latin-1 'ä' = 0xE4) with U+FFFD,
//! - and `MessagePart::contents()` always returns the decoded string re-encoded as
//!   UTF-8, so even a successfully-decoded charset (e.g. UTF-16LE) loses its BOM and
//!   its original encoding entirely.
//!
//! `is_encoding_problem` does not flag any of this: it's only set for malformed
//! message structure, never for a lossy charset fallback.

use mail_parser::decoders::base64::base64_decode;
use mail_parser::decoders::quoted_printable::quoted_printable_decode;
use mail_parser::{Encoding, Message, MessagePart, MimeHeaders};

/// Returns a part's bytes with only the `Content-Transfer-Encoding` undone, never
/// the charset decoding. CRLF becomes LF in non-base64 `text/*` parts.
pub fn attachment_bytes(message: &Message<'_>, part: &MessagePart<'_>) -> Vec<u8> {
    let raw = &message.raw_message()[part.offset_body as usize..part.offset_end as usize];
    let bytes = match part.encoding {
        Encoding::Base64 => return base64_decode(raw).unwrap_or_default(),
        Encoding::QuotedPrintable => quoted_printable_decode(raw).unwrap_or_default(),
        Encoding::None => raw.to_vec(),
    };
    if is_text(part) {
        crlf_to_lf(bytes)
    } else {
        bytes
    }
}

fn is_text(part: &MessagePart<'_>) -> bool {
    part.content_type()
        .is_none_or(|ct| ct.ctype().eq_ignore_ascii_case("text"))
}

fn crlf_to_lf(bytes: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        if b != b'\r' || bytes.get(i + 1) != Some(&b'\n') {
            out.push(b);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use mail_parser::MessageParser;

    fn build_eml(content_type: &str, cte: &str, body: &[u8]) -> Vec<u8> {
        let mut eml = format!(
            "From: a@example.com\r\n\
             Date: Mon, 1 Jan 2024 00:00:00 +0000\r\n\
             Message-ID: <1@example.com>\r\n\
             Mime-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"BOUNDARY\"\r\n\
             \r\n\
             --BOUNDARY\r\n\
             Content-Type: text/plain\r\n\
             \r\n\
             body\r\n\
             --BOUNDARY\r\n\
             Content-Type: {content_type}\r\n\
             Content-Disposition: attachment; filename=\"data.csv\"\r\n\
             Content-Transfer-Encoding: {cte}\r\n\
             \r\n"
        )
        .into_bytes();
        eml.extend_from_slice(body);
        eml.extend_from_slice(b"\r\n--BOUNDARY--\r\n");
        eml
    }

    fn first_attachment_bytes(eml: &[u8]) -> Vec<u8> {
        let message = MessageParser::default()
            .parse(eml)
            .expect("fixture message must parse");
        let part = message
            .attachments()
            .next()
            .expect("fixture message must have an attachment");
        super::attachment_bytes(&message, part)
    }

    fn utf16le_with_bom(s: &str) -> Vec<u8> {
        let mut out = vec![0xFF, 0xFE];
        for unit in s.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out
    }

    /// Minimal RFC 4648 base64 encoder, just so tests can build a realistic
    /// `Content-Transfer-Encoding: base64` fixture without adding a dependency.
    fn base64_encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0];
            let b1 = *chunk.get(1).unwrap_or(&0);
            let b2 = *chunk.get(2).unwrap_or(&0);
            let n = (b0 as u32) << 16 | (b1 as u32) << 8 | b2 as u32;
            out.push(ALPHABET[(n >> 18 & 0x3F) as usize] as char);
            out.push(ALPHABET[(n >> 12 & 0x3F) as usize] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(n >> 6 & 0x3F) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[(n & 0x3F) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    #[test]
    fn text_qp_crlf_becomes_lf() {
        let body: &[u8] = b"=EF=BB=BFname,city\r\nHans,Wi=\r\nen\r\nTotal,=C2=A391\r\n";
        let eml = build_eml("text/csv", "quoted-printable", body);

        let got = first_attachment_bytes(&eml);
        assert_eq!(
            got,
            "\u{feff}name,city\nHans,Wien\nTotal,\u{a3}91\n".as_bytes()
        );
    }

    #[test]
    fn text_qp_bare_lf_is_untouched() {
        let eml = build_eml("text/csv", "quoted-printable", b"a\nb\r\nc");

        assert_eq!(first_attachment_bytes(&eml), b"a\nb\nc".to_vec());
    }

    #[test]
    fn text_8bit_lone_cr_is_untouched() {
        let eml = build_eml("text/csv", "8bit", b"a\rb\r\nc");

        assert_eq!(first_attachment_bytes(&eml), b"a\rb\nc".to_vec());
    }

    #[test]
    fn text_8bit_crlf_becomes_lf() {
        let eml = build_eml("text/csv", "8bit", b"a,b\r\nc,d\r\n");

        assert_eq!(first_attachment_bytes(&eml), b"a,b\nc,d\n".to_vec());
    }

    #[test]
    fn text_base64_crlf_is_preserved() {
        let body: &[u8] = b"a,b\r\nc,d\r\n";
        let eml = build_eml("text/csv", "base64", base64_encode(body).as_bytes());

        assert_eq!(first_attachment_bytes(&eml), body.to_vec());
    }

    #[test]
    fn attachment_latin1_bytes_without_charset_are_preserved() {
        // No `charset` parameter is declared; the body is raw Latin-1
        // ("Fernwärme", 0xE4 = 'ä'), sent as 8bit so no transfer decoding applies.
        let body: &[u8] = b"Fernw\xe4rme\n";
        let eml = build_eml("text/csv", "8bit", body);

        let got = first_attachment_bytes(&eml);
        assert_eq!(got, body.to_vec(), "non-UTF-8 bytes must not become U+FFFD");
    }

    #[test]
    fn attachment_utf16le_bom_is_preserved() {
        // Real UTF-16LE bytes including the BOM.
        let body = utf16le_with_bom("Fernwärme");
        let eml = build_eml("text/csv; charset=utf-16le", "8bit", &body);

        let got = first_attachment_bytes(&eml);
        assert_eq!(got, body, "UTF-16LE bytes and BOM must survive untouched");
    }

    #[test]
    fn attachment_binary_8bit_passthrough_is_unaffected() {
        let body: &[u8] = b"\x00\x01\xFEPDF\r\nsome\x00binary\nend\xFF";
        let eml = build_eml("application/octet-stream", "8bit", body);

        let got = first_attachment_bytes(&eml);
        assert_eq!(got, body.to_vec(), "binary attachments must be untouched");
    }

    #[test]
    fn binary_qp_crlf_is_preserved() {
        let eml = build_eml("application/octet-stream", "quoted-printable", b"a\r\nb");

        assert_eq!(first_attachment_bytes(&eml), b"a\r\nb".to_vec());
    }

    #[test]
    fn attachment_binary_base64_roundtrips() {
        let body: &[u8] = b"\x00\x01\xFEPDF\r\nsome\x00binary\nend\xFF";
        let eml = build_eml(
            "application/pdf",
            "base64",
            &base64_encode(body).into_bytes(),
        );

        let got = first_attachment_bytes(&eml);
        assert_eq!(
            got,
            body.to_vec(),
            "base64-transported binary must roundtrip"
        );
    }
}
