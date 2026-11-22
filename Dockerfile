# Stage 1: Build our pure no_std binary
FROM rust:alpine AS builder
RUN apk add --no-cache lld
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
RUN rustup target add x86_64-unknown-linux-musl && cargo build --release

# Stage 2: Empty container — literally ONLY our ~22 KB binary!
FROM scratch
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/umsg /umsg
ENV PORT=8080 \
    LOG_PATH=/data/notifications.log \
    KEEPALIVE_SECS=30
EXPOSE 8080
VOLUME ["/data"]
ENTRYPOINT ["/umsg"]
