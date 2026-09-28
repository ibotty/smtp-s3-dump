use anyhow::{Context, Result};
use aws_sdk_s3::primitives::ByteStream;
use futures::future::try_join_all;
use mail_parser::{Message, MessagePart, MimeHeaders};
use serde_json::{json, Map, Value};

use crate::attachment;
use crate::db;
use crate::smtp::Config;

struct Upload {
    key: String,
    body: Vec<u8>,
    content_type: Option<String>,
}

pub struct UploadPlan {
    pub message_id: String,
    uploads: Vec<Upload>,
    pub body_text: String,
    pub body_html: String,
    pub headers: Value,
    pub attachments: Value,
}

fn plan_uploads(rcpt: &str, from: &str, message: &Message<'_>) -> Result<UploadPlan> {
    let message_id = message.message_id().context("mail has no message id")?;
    let date = message.date().context("mail has no date")?.to_rfc3339();
    let base_path = format!(
        "{}/{}/{}-{}/",
        sanitize_key_component(&rcpt.to_lowercase()),
        sanitize_key_component(from),
        sanitize_key_component(&date),
        sanitize_key_component(message_id),
    );

    let mut uploads = vec![];
    let mut attachments = vec![];
    for (ix, part) in message.attachments().enumerate() {
        let filename = sanitize_filename(part.attachment_name().unwrap_or_default());
        let key = format!("{}attachments/{:02}-{}", base_path, ix, filename);
        let part_type = part
            .content_type()
            .and_then(|ct| Some(format!("{}/{}", ct.ctype(), ct.subtype()?).to_lowercase()));
        let content_type = content_type(&key, part_type);
        attachments.push(json!({
            "index": ix,
            "filename": filename,
            "rel_path": key,
            "content_type": content_type,
        }));
        uploads.push(Upload {
            body: attachment::attachment_bytes(message, part),
            key,
            content_type,
        });
    }

    let headers = headers_to_json(message.headers_raw());
    let mut push = |name: &str, body: Vec<u8>| {
        let key = format!("{base_path}{name}");
        uploads.push(Upload {
            content_type: content_type_from_path(&key),
            key,
            body,
        });
    };
    push("headers.json", serde_json::to_vec_pretty(&headers)?);

    // this selects only the first part
    let body_text = message.text_bodies().next();
    if let Some(part) = body_text {
        push("body.txt", part.contents().to_vec());
    }

    // this selects only the first part
    let body_html = message.html_bodies().next();
    if let Some(part) = body_html {
        push("body.html", part.contents().to_vec());
    }

    let trimmed = |part: Option<&MessagePart<'_>>| {
        part.and_then(MessagePart::text_contents)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    Ok(UploadPlan {
        message_id: message_id.to_string(),
        uploads,
        body_text: trimmed(body_text),
        body_html: trimmed(body_html),
        headers,
        attachments: Value::Array(attachments),
    })
}

pub async fn upload_message(
    config: &Config,
    from: &str,
    rcpt: &str,
    message: &Message<'_>,
) -> Result<()> {
    let mut plan = plan_uploads(rcpt, from, message)?;
    let uploads = std::mem::take(&mut plan.uploads);
    try_join_all(
        uploads
            .into_iter()
            .map(|u| upload_file(&config.s3, &config.bucket, u)),
    )
    .await?;

    // afterwards, when complete, insert into DB
    db::insert_mail(&config.pg_pool, rcpt, from, &plan).await
}

/// Headers as a JSON object of `name -> value`. A header that occurs once stays a plain string
/// (backward compatible); one that occurs several times becomes an array of all its values in
/// message order, so nothing (e.g. a `Received` chain) is silently dropped. Cross-name order is
/// not kept (serde_json objects are sorted).
fn headers_to_json<'a>(headers: impl Iterator<Item = (&'a str, &'a str)>) -> Value {
    let mut map = Map::new();
    for (k, v) in headers {
        let v = Value::String(v.trim().to_string());
        match map.get_mut(k) {
            None => {
                map.insert(k.to_string(), v);
            }
            Some(Value::Array(a)) => a.push(v),
            Some(old) => *old = Value::Array(vec![old.take(), v]),
        }
    }
    Value::Object(map)
}

fn content_type_from_path(path: &str) -> Option<String> {
    mime_guess::from_path(path).first_raw().map(str::to_string)
}

/// The extension-based guess wins; the part's own `Content-Type` is the fallback.
fn content_type(path: &str, part_type: Option<String>) -> Option<String> {
    content_type_from_path(path).or(part_type)
}

/// Makes an attachment name safe to embed in an S3 key: drops `/` and `\\`, collapses runs of
/// dots and strips leading ones (so no `..` or hidden files), falls back to `attachment`.
/// Single dots are kept, since the extension drives the guessed content type.
fn sanitize_filename(name: &str) -> String {
    let out = collapse_dots(name.chars().filter(|c| !is_unsafe_key_char(*c)));
    if out.is_empty() {
        "attachment".to_string()
    } else {
        out
    }
}

fn is_unsafe_key_char(c: char) -> bool {
    matches!(c, '/' | '\\') || c.is_control()
}

/// Collapses runs of dots, strips leading ones and trims whitespace.
fn collapse_dots(chars: impl Iterator<Item = char>) -> String {
    let mut out = String::new();
    for c in chars {
        if c == '.' && (out.is_empty() || out.ends_with('.')) {
            continue;
        }
        out.push(c);
    }
    out.trim().to_string()
}

/// Makes an attacker-controlled value (recipient, sender, Message-ID) safe as part of an S3 key:
/// `/`, `\\` and control characters (incl. NUL) become `_`, so it can never add path segments,
/// and dot runs are collapsed so no `..` survives. Falls back to `unknown` if nothing is left.
fn sanitize_key_component(s: &str) -> String {
    let out = collapse_dots(
        s.chars()
            .map(|c| if is_unsafe_key_char(c) { '_' } else { c }),
    );
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

async fn upload_file(s3_client: &aws_sdk_s3::Client, bucket: &str, u: Upload) -> Result<()> {
    s3_client
        .put_object()
        .bucket(bucket)
        .body(ByteStream::from(u.body))
        .set_content_type(u.content_type)
        .key(&u.key)
        .send()
        .await
        .map_err(aws_sdk_s3::Error::from)
        .with_context(|| format!("upload {}", u.key))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        content_type, headers_to_json, plan_uploads, sanitize_filename, sanitize_key_component,
    };
    use mail_parser::MessageParser;
    use serde_json::json;

    const EML: &str = concat!(
        "Received: from a\r\n",
        "Received: from b\r\n",
        "Message-ID: <id.1@example.org>\r\n",
        "Date: Mon, 1 Jan 2024 00:00:00 +0000\r\n",
        "Subject: hi\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/mixed; boundary=\"m\"\r\n",
        "\r\n",
        "--m\r\n",
        "Content-Type: multipart/alternative; boundary=\"a\"\r\n",
        "\r\n",
        "--a\r\n",
        "Content-Type: text/plain\r\n",
        "\r\n",
        " plain body \r\n",
        "--a\r\n",
        "Content-Type: text/html\r\n",
        "\r\n",
        "<p>html body</p>\r\n",
        "--a--\r\n",
        "--m\r\n",
        "Content-Type: application/pdf; name=\"a.pdf\"\r\n",
        "Content-Disposition: attachment; filename=\"a.pdf\"\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n",
        "AAECAw==\r\n",
        "--m\r\n",
        "Content-Type: application/x-custom\r\n",
        "Content-Disposition: attachment; filename=\"../evil\"\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n",
        "aGk=\r\n",
        "--m\r\n",
        "Content-Type: application/octet-stream\r\n",
        "Content-Disposition: attachment\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n",
        "AQ==\r\n",
        "--m--\r\n",
    );

    fn plan(rcpt: &str, from: &str, eml: &str) -> anyhow::Result<super::UploadPlan> {
        plan_uploads(
            rcpt,
            from,
            &MessageParser::default().parse(eml.as_bytes()).unwrap(),
        )
    }

    #[test]
    fn plans_keys_bodies_and_content_types() {
        let p = plan("Alice@Example.org", "bob@example.org", EML).unwrap();
        let base = "alice@example.org/bob@example.org/2024-01-01T00:00:00Z-id.1@example.org/";
        let got: Vec<_> = p
            .uploads
            .iter()
            .map(|u| (u.key.as_str(), u.content_type.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    format!("{base}attachments/00-a.pdf"),
                    Some("application/pdf")
                ),
                (
                    format!("{base}attachments/01-evil"),
                    Some("application/x-custom")
                ),
                (
                    format!("{base}attachments/02-attachment"),
                    Some("application/octet-stream")
                ),
                (format!("{base}headers.json"), Some("application/json")),
                (format!("{base}body.txt"), Some("text/plain")),
                (format!("{base}body.html"), Some("text/html")),
            ]
            .iter()
            .map(|(k, c)| (k.as_str(), *c))
            .collect::<Vec<_>>()
        );
        assert_eq!(p.message_id, "id.1@example.org");
        assert_eq!(p.body_text, "plain body");
        assert_eq!(p.body_html, "<p>html body</p>");
    }

    #[test]
    fn plans_attachment_wire_bytes_and_metadata() {
        let p = plan("a@b", "c@d", EML).unwrap();
        let bodies: Vec<_> = p.uploads[..3].iter().map(|u| u.body.as_slice()).collect();
        assert_eq!(bodies, [&[0u8, 1, 2, 3][..], b"hi", &[1]]);
        let base = "a@b/c@d/2024-01-01T00:00:00Z-id.1@example.org/attachments";
        assert_eq!(
            p.attachments,
            json!([
                {"index": 0, "filename": "a.pdf", "rel_path": format!("{base}/00-a.pdf"), "content_type": "application/pdf"},
                {"index": 1, "filename": "evil", "rel_path": format!("{base}/01-evil"), "content_type": "application/x-custom"},
                {"index": 2, "filename": "attachment", "rel_path": format!("{base}/02-attachment"), "content_type": "application/octet-stream"},
            ])
        );
    }

    #[test]
    fn plans_headers_matching_headers_json() {
        let p = plan("a@b", "c@d", EML).unwrap();
        assert_eq!(p.headers["Subject"], "hi");
        assert_eq!(p.headers["Received"], json!(["from a", "from b"]));
        let stored = p
            .uploads
            .iter()
            .find(|u| u.key.ends_with("/headers.json"))
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&stored.body).unwrap(),
            p.headers
        );
    }

    #[test]
    fn plans_without_attachments() {
        let eml = "Message-ID: <a@b>\r\nDate: Mon, 1 Jan 2024 00:00:00 +0000\r\n\r\n";
        let p = plan("r@x", "s@y", eml).unwrap();
        let keys: Vec<_> = p.uploads.iter().map(|u| u.key.as_str()).collect();
        assert!(keys.iter().all(|k| !k.contains("attachments/")));
        assert!(keys.iter().any(|k| k.ends_with("/headers.json")));
        assert_eq!(p.attachments, json!([]));
    }

    #[test]
    fn plans_hostile_components_into_fixed_depth() {
        let eml = "Message-ID: <../../x/y>\r\nDate: Mon, 1 Jan 2024 00:00:00 +0000\r\n\r\nhi\r\n";
        let p = plan("../r@x", "\"../s\"@y", eml).unwrap();
        for u in &p.uploads {
            assert_eq!(u.key.matches('/').count(), 3, "{}", u.key);
            assert!(!u.key.contains(".."), "{}", u.key);
        }
    }

    #[test]
    fn plan_requires_message_id_and_date() {
        let no_id = "Date: Mon, 1 Jan 2024 00:00:00 +0000\r\n\r\nhi\r\n";
        let no_date = "Message-ID: <a@b>\r\n\r\nhi\r\n";
        assert!(plan("a@b", "c@d", no_id).is_err());
        assert!(plan("a@b", "c@d", no_date).is_err());
    }

    #[test]
    fn content_type_prefers_extension_then_part_type() {
        let part = Some("application/x-custom".to_string());
        assert_eq!(
            content_type("k/00-a.pdf", part.clone()).as_deref(),
            Some("application/pdf")
        );
        assert_eq!(
            content_type("k/00-attachment", part).as_deref(),
            Some("application/x-custom")
        );
        assert_eq!(content_type("k/00-attachment", None), None);
        assert_eq!(
            content_type("k/00-a.png", None).as_deref(),
            Some("image/png")
        );
    }

    #[test]
    fn sanitizes_key_components() {
        for (input, want) in [
            ("alice@example.org", "alice@example.org"),
            ("john.smith@example.org", "john.smith@example.org"),
            ("<abc.123@mail.example.org>", "<abc.123@mail.example.org>"),
            ("../../etc/passwd", "_._etc_passwd"),
            ("a/../b", "a_._b"),
            ("..", "unknown"),
            ("/", "_"),
            ("a\\b", "a_b"),
            ("\"../x/y\"@evil.org", "\"._x_y\"@evil.org"),
            ("a\0b\r\nc\x1b", "a_b__c_"),
            ("", "unknown"),
            ("   ", "unknown"),
            ("\0", "_"),
        ] {
            let got = sanitize_key_component(input);
            assert_eq!(got, want, "input {input:?}");
            assert!(!got.contains('/') && !got.contains(".."), "{got:?}");
            assert!(!got.chars().any(char::is_control), "{got:?}");
        }
    }

    #[test]
    fn filenames_drop_control_chars() {
        assert_eq!(sanitize_filename("a\0b\n.txt"), "ab.txt");
    }

    #[test]
    fn duplicate_headers_are_preserved() {
        let raw = [
            ("Subject", " hi\r\n"),
            ("Received", " from a"),
            ("Message-ID", " <1@x>"),
            ("Received", " from b"),
            ("Received", " from c"),
        ];
        assert_eq!(
            headers_to_json(raw.into_iter()),
            json!({
                "Subject": "hi",
                "Message-ID": "<1@x>",
                "Received": ["from a", "from b", "from c"],
            })
        );
    }

    #[test]
    fn sanitizes_filenames() {
        for (input, want) in [
            ("report.pdf", "report.pdf"),
            ("archive.tar.gz", "archive.tar.gz"),
            ("../../etc/passwd", "etcpasswd"),
            ("a/b\\c.txt", "abc.txt"),
            ("..hidden", "hidden"),
            ("a..b.txt", "a.b.txt"),
            ("...", "attachment"),
            ("/", "attachment"),
            ("   ", "attachment"),
            ("", "attachment"),
        ] {
            assert_eq!(sanitize_filename(input), want, "input {input:?}");
        }
    }
}
