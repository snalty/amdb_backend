FROM rust:latest AS build

ARG APP_NAME=alphamissense_web

RUN --mount=type=bind,source=src,target=src \
    --mount=type=bind,source=Cargo.toml,target=Cargo.toml \
    --mount=type=bind,source=Cargo.lock,target=Cargo.lock \
    --mount=type=bind,source=sqlx-data.json,target=sqlx-data.json \
    --mount=type=cache,target=/app/target/ \
    --mount=type=cache,target=/usr/local/cargo/registry/ \
    <<EOF
set -e
cargo build --locked --release
cp ./target/release/$APP_NAME /bin/amdb
EOF

FROM debian:trixie-slim AS final

COPY --from=build /bin/amdb /bin/

# Expose the port that the application listens on.
EXPOSE 8000

CMD ["/bin/amdb"]

