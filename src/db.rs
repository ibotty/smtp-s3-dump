use anyhow::{Context, Result};
use sqlx::postgres::PgPool;

use crate::s3::UploadPlan;

/// `from` is the client-asserted `MAIL FROM`: it is not authenticated (no SMTP AUTH, SPF or DKIM),
/// so consumers of the stored `"from"` column must not trust it.
pub async fn insert_mail(pool: &PgPool, rcpt: &str, from: &str, plan: &UploadPlan) -> Result<()> {
    sqlx::query!(
        r#"INSERT INTO data_gateways.smtp_gateway
            (message_id, "to", "from", body_text, body_html, headers, attachments)
            VALUES ($1, $2, $3, $4, $5, $6, $7);"#,
        plan.message_id,
        rcpt,
        from,
        plan.body_text,
        plan.body_html,
        plan.headers,
        plan.attachments
    )
    .execute(pool)
    .await
    .context("insert mail")?;
    Ok(())
}

pub async fn check_address(pool: &PgPool, from: &str, rcpt: &str) -> Result<bool> {
    let query = sqlx::query!(r#"SELECT is_valid_rcpt($1, $2) AS "b!";"#, rcpt, from);
    let res = query.fetch_one(pool).await.context("check address")?;
    Ok(res.b)
}
