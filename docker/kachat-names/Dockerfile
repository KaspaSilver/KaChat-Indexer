# .kachat Domains server — the names registry on its own (docs/KACHAT_NAMES_STANDALONE.md).
# Runs the names follower, the address-profiles follower and the webserver in names-only mode
# (KACHAT_WEBSERVER_ONLY=names) under supervisord. Needs a Kaspa node (wRPC Borsh) and a
# Postgres database, nothing else: no chat indexer, no KaPosts, no block ingester.
# Build context is the repo root (../..).

FROM rust:1.91-alpine AS build
RUN apk add --no-cache musl-dev pkgconfig openssl-dev openssl-libs-static
WORKDIR /src
# The committed lockfile pins dependency versions (kachat-audits IDX-010). No --locked: this image
# builds a subset of the workspace, so cargo drops the other members' entries from its copy.
COPY Cargo.toml Cargo.lock ./
RUN sed -i '/members = \[/,/\]/c\members = ["kachat-webserver","kachat-names","kachat-names-follower"]' Cargo.toml
COPY kachat-webserver ./kachat-webserver
COPY kachat-names ./kachat-names
COPY kachat-names-follower ./kachat-names-follower
RUN cargo build --release --bin kachat-webserver --bin kachat-names-follower

FROM alpine:3.20
RUN apk add --no-cache ca-certificates libssl3 libcrypto3 libgcc supervisor
# Unprivileged user for every service (kachat-audits IDX-011); see supervisord.conf.
RUN addgroup -S -g 10001 kachat && adduser -S -D -H -u 10001 -G kachat kachat
WORKDIR /app
COPY --from=build /src/target/release/kachat-webserver /app/
COPY --from=build /src/target/release/kachat-names-follower /app/
COPY docker/kachat-names/app/ /app/
COPY docker/kachat-names/supervisord.conf /etc/supervisord.conf
RUN chmod +x /app/run-*.sh
ENV KACHAT_WEBSERVER_ONLY=names
EXPOSE 3080
ENTRYPOINT ["supervisord", "-c", "/etc/supervisord.conf"]
