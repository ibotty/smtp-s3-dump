use anyhow::Result;
use serde_json::Value;
use sqlx::postgres::PgPool;
use tracing::{instrument, trace};

#[allow(clippy::too_many_arguments)]
#[instrument(skip_all, fields(from, rcpt))]
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

#[instrument(skip(pool))]
pub async fn check_address(pool: &PgPool, from: &str, rcpt: &str) -> Result<bool> {
    trace!("checking DB");
    let query = sqlx::query!(r#"SELECT is_valid_rcpt($1, $2) AS "b!";"#, rcpt, from);
    let res = query.fetch_one(pool).await?;
    trace!("checked DB, got {}", res.b);
    Ok(res.b)
}
