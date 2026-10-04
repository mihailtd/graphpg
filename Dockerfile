# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS build
WORKDIR /src
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked --bin graphpg

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /app graphpg
COPY --from=build /src/target/release/graphpg /usr/local/bin/graphpg
WORKDIR /app
USER graphpg
ENV PORT=4000
EXPOSE 4000
ENTRYPOINT ["graphpg"]
