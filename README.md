# SMTP microservice to get mail and store in S3

This is a SMTP server meant to be deployed behind a real MTA (postfix) to accept mail for programmatic consumption.
It explodes mail to S3 with metadata, attachments (mime parts).

## automatic consumption of S3 data
It might emit a CloudEvent eventually, but for now use s3 bucket notifications.
