use anyhow::Result;
use serde_json::Value;
use sqlx::postgres::PgPool;

pub async fn insert_mail(
    pool: &PgPool,
    message_id: &str,
    rcpt: &str,
    from: &str,
    body_text: &str,
    body_html: &str,
    headers: Value,
    attachments: Value,
) -> Result<()> {
    trace!("inserting into DB");
    let query = sqlx::query!(
        r#"INSERT INTO data_gateways.smtp_gateway
            (message_id, "to", "from", body_text, body_html, headers, attachments)
            VALUES ($1, $2, $3, $4, $5, $6, $7);"#,
        message_id,
        rcpt,
        from,
        body_text,
        body_html,
        headers,
        attachments
    );

    let _ = query.execute(pool).await?;
    Ok(())
}
