# Headless Den server (no GUI, no audio libraries).
# Build from the repo root:  docker build -f deploy/Dockerfile -t den .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY assets assets
RUN cargo build --release --locked -p denchat --no-default-features

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates python3 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/den /usr/local/bin/den
ENV DEN_HOME=/data
VOLUME /data
EXPOSE 8245/udp 8245/tcp
ENTRYPOINT ["den"]
CMD ["serve", "--no-console"]
