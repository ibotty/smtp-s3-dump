use std::collections::HashMap;

use anyhow::{Context, Result};
use aws_sdk_s3::primitives::ByteStream;
use futures::future::try_join_all;
use mail_parser::{Message, MessagePart, MimeHeaders};
use serde_json::json;
use sqlx::PgPool;
use tracing::{instrument, trace};

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
    trace!("uploading message");

    let message_id = message.message_id().context("mail has no message id")?;
    let date = message.date().context("mail has no date")?.to_rfc3339();
    let base_path = format!("{}/{}/{}-{}/", rcpt.to_lowercase(), from, date, message_id);

    let s3_client = aws_sdk_s3::Client::from_conf(s3_config.clone());

    // attachments uploads
    let mut attachments_metadata = vec![];
    let mut uploads = message
        .attachments()
        .enumerate()
        .map(|(ix, attachment)| {
            let attachment_name = attachment
                .attachment_name()
                .context("attachment has no name")?;
            let body = attachment.contents();
            let path = format!("{}attachments/{:02}-{}", base_path, ix, attachment_name);

            let metadata = json!({
                "index": ix,
                "filename": attachment_name,
                "rel_path": path,
                "content_type": mime_guess::from_path(&path).first_raw(),
            });

            attachments_metadata.push(metadata);

            Ok(upload_file(&s3_client, bucket, path, body.to_vec()))
        })
        .collect::<Result<Vec<_>>>()?;

    let headers_map: HashMap<&str, &str> =
        message.headers_raw().map(|(k, v)| (k, v.trim())).collect();
    let headers_json = serde_json::to_vec_pretty(&headers_map)?;
    let headers_path = format!("{}headers.json", base_path);
    uploads.push(upload_file(&s3_client, bucket, headers_path, headers_json));

    // this selects only the first part
    let body_text = message.text_bodies().next();
    if let Some(body_text) = body_text {
        let body_text_path = format!("{}body.txt", base_path);
        uploads.push(upload_file(
            &s3_client,
            bucket,
            body_text_path,
            body_text.contents().to_vec(),
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
        ));
    }

    // run upload futures
    try_join_all(uploads).await?;

    // afterwards, when complete, insert into DB
    db::insert_mail(
        pg_pool,
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

#[instrument(skip(s3_client, body))]
async fn upload_file(
    s3_client: &aws_sdk_s3::Client,
    bucket: &str,
    path: String,
    body: Vec<u8>,
) -> Result<()> {
    let content_type = mime_guess::from_path(&path).first_raw();

    trace!(
        "uploading file path={} content_type={}",
        path,
        content_type.unwrap_or("")
    );

    let s3_req = s3_client
        .put_object()
        .bucket(bucket)
        .body(ByteStream::from(body))
        .set_content_type(content_type.map(str::to_string))
        .key(path);

    s3_req.send().await.map_err(aws_sdk_s3::Error::from)?;
    Ok(())
}
