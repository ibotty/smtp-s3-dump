FROM registry.access.redhat.com/hi/static:latest@sha256:cac22d5a26d90c14801f8e48cc99dcb8a08f270cdc9ebaa205ac5b29f8a3d2fa
ARG BINARY=target/x86_64-unknown-linux-musl/release/smtp-s3-dump
ARG VERSION

LABEL maintainer="Tobias Florek <tob@butter.sh>" \
      org.opencontainers.image.title="smtp-s3-dump" \
      org.opencontainers.image.description="SMTP server that dumps received mail to S3" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.licenses="GPL-3.0-or-later" \
      org.opencontainers.image.source="https://github.com/ibotty/smtp-s3-dump" \
      org.opencontainers.image.authors="Tobias Florek <tob@butter.sh>"

EXPOSE 2525/tcp 2526/tcp

COPY $BINARY /smtp-s3-dump

CMD ["/smtp-s3-dump"]
