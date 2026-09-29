CREATE OR REPLACE FUNCTION is_valid_rcpt(rcpt TEXT, "from" TEXT)
RETURNS BOOLEAN AS $func$
SELECT EXISTS(
  SELECT 1 
  FROM data_gateways.smtp_gateway_recipient
  WHERE smtp_gateway_recipient.inbound_email_address = rcpt
    AND "from" LIKE smtp_gateway_recipient.from_pattern);
$func$ LANGUAGE sql
  STABLE LEAKPROOF STRICT;
