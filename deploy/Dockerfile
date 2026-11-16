# Headless tailchat server (no GUI, no audio libraries).
# Build from the repo root:  docker build -f deploy/Dockerfile -t tailchat .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY assets assets
RUN cargo build --release --locked -p tailchat --no-default-features

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates python3 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/tailchat /usr/local/bin/tailchat
ENV TAILCHAT_HOME=/data
VOLUME /data
EXPOSE 8245/udp 8245/tcp
ENTRYPOINT ["tailchat"]
CMD ["serve", "--no-console"]
