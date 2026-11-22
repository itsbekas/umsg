FROM rust:alpine AS builder
RUN apk add --no-cache lld
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
RUN rustup target add x86_64-unknown-linux-musl && cargo build --release

FROM scratch
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/notifications-1mb /notifications-1mb
EXPOSE 8080
VOLUME ["/data"]
ENTRYPOINT ["/notifications-1mb"]
