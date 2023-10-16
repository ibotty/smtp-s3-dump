FROM registry.access.redhat.com/ubi9-minimal
ARG BINARY=target/release/smtp-s3-dump

LABEL maintainer="Tobias Florek <tob@butter.sh>"

EXPOSE 2525/tcp

COPY $BINARY /smtp-s3-dump

CMD /smtp-s3-dump
USER 1000
