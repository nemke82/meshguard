# Enrollment service for the MeshGuard relay (ADR-007 §3).
FROM docker.io/library/rust:1.100-alpine3.22 AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM docker.io/library/alpine:3.22
RUN addgroup -S -g 1884 enroll && adduser -S -D -H -u 1884 -G enroll enroll \
 && mkdir /data && chown enroll:enroll /data
COPY --from=build /src/target/release/meshguard-relay-enroll /usr/local/bin/
USER enroll
ENTRYPOINT ["/usr/local/bin/meshguard-relay-enroll"]
