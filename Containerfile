FROM registry.access.redhat.com/ubi9-minimal
ARG BINARY=target/release/smtp-s3-dump
ARG VERSION

LABEL maintainer="Tobias Florek <tob@butter.sh>" \
      org.opencontainers.image.title="smtp-s3-dump" \
      org.opencontainers.image.description="SMTP server that dumps received mail to S3" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.licenses="GPL-3.0-or-later" \
      org.opencontainers.image.source="https://github.com/ibotty/smtp-s3-dump" \
      org.opencontainers.image.authors="Tobias Florek <tob@butter.sh>"

EXPOSE 2525/tcp

COPY $BINARY /smtp-s3-dump

CMD /smtp-s3-dump
USER 1000
