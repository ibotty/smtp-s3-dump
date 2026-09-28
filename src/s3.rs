use std::collections::HashMap;

use anyhow::{Context, Result};
use aws_sdk_s3::primitives::ByteStream;
use futures::future::try_join_all;
use mail_parser::{Message, MessagePart, MimeHeaders};
use serde_json::json;
use sqlx::PgPool;
use tracing::{debug, instrument};

use crate::attachment;
use crate::db;

#[instrument(skip(s3_config, message, pg_pool), fields(message_id = message.message_id()))]
pub async fn upload_message(
    s3_config: &aws_sdk_s3::Config,
    pg_pool: &PgPool,
    bucket: &str,
    from: &str,
    rcpt: &str,
    message: Message<'_>,
) -> Result<()> {
    debug!("uploading message");

    let message_id = message.message_id().context("mail has no message id")?;
    let date = message.date().context("mail has no date")?.to_rfc3339();
    let base_path = format!("{}/{}/{}-{}/", rcpt.to_lowercase(), from, date, message_id);

    let s3_client = aws_sdk_s3::Client::from_conf(s3_config.clone());

    // attachments uploads
    let mut attachments_metadata = vec![];
    let mut uploads = message
        .attachments()
        .enumerate()
        .map(|(ix, part)| {
            let attachment_name = sanitize_filename(part.attachment_name().unwrap_or_default());
            let body = attachment::attachment_bytes(&message, part);
            let path = format!("{}attachments/{:02}-{}", base_path, ix, attachment_name);
            let part_type = part
                .content_type()
                .and_then(|ct| Some(format!("{}/{}", ct.ctype(), ct.subtype()?).to_lowercase()));
            let content_type = content_type(&path, part_type);

            let metadata = json!({
                "index": ix,
                "filename": attachment_name,
                "rel_path": path,
                "content_type": content_type,
            });

            attachments_metadata.push(metadata);

            Ok(upload_file(&s3_client, bucket, path, body, content_type))
        })
        .collect::<Result<Vec<_>>>()?;

    let headers_map: HashMap<&str, &str> =
        message.headers_raw().map(|(k, v)| (k, v.trim())).collect();
    let headers_json = serde_json::to_vec_pretty(&headers_map)?;
    let headers_path = format!("{}headers.json", base_path);
    uploads.push(upload_file(
        &s3_client,
        bucket,
        headers_path,
        headers_json,
        None,
    ));

    // this selects only the first part
    let body_text = message.text_bodies().next();
    if let Some(body_text) = body_text {
        let body_text_path = format!("{}body.txt", base_path);
        uploads.push(upload_file(
            &s3_client,
            bucket,
            body_text_path,
            body_text.contents().to_vec(),
            None,
        ));
    }

    // this selects only the first part
    let body_html = message.html_bodies().next();
    if let Some(body_html) = body_html {
        let body_html_path = format!("{}body.html", base_path);
        uploads.push(upload_file(
            &s3_client,
            bucket,
            body_html_path,
            body_html.contents().to_vec(),
            None,
        ));
    }

    // run upload futures
    try_join_all(uploads).await?;

    // afterwards, when complete, insert into DB
    db::insert_mail(
        pg_pool,
        message_id,
        rcpt,
        from,
        body_text
            .and_then(MessagePart::text_contents)
            .unwrap_or("")
            .trim(),
        body_html
            .and_then(MessagePart::text_contents)
            .unwrap_or("")
            .trim(),
        serde_json::to_value(headers_map)?,
        serde_json::to_value(attachments_metadata)?,
    )
    .await?;
    Ok(())
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
    let mut out = String::with_capacity(name.len());
    for c in name.chars().filter(|c| !matches!(c, '/' | '\\')) {
        if c == '.' && (out.is_empty() || out.ends_with('.')) {
            continue;
        }
        out.push(c);
    }
    let out = out.trim().to_string();
    if out.is_empty() {
        "attachment".to_string()
    } else {
        out
    }
}

#[instrument(skip(s3_client, body))]
async fn upload_file(
    s3_client: &aws_sdk_s3::Client,
    bucket: &str,
    path: String,
    body: Vec<u8>,
    content_type: Option<String>,
) -> Result<()> {
    let content_type = content_type.or_else(|| content_type_from_path(&path));

    debug!(
        "uploading file path={} content_type={}",
        path,
        content_type.as_deref().unwrap_or("")
    );

    let s3_req = s3_client
        .put_object()
        .bucket(bucket)
        .body(ByteStream::from(body))
        .set_content_type(content_type)
        .key(path);

    s3_req.send().await.map_err(aws_sdk_s3::Error::from)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{content_type, sanitize_filename};

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
