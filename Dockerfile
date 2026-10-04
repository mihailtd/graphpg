# syntax=docker/dockerfile:1

# --- Build: a fully static musl binary -------------------------------------------------
# Base images are pinned by digest; Dependabot proposes updates.
FROM rust:1.98.1-alpine@sha256:7cc1c22d77d9432f7fe012a70e6d3e555af54c2a6832700ed7d553f1769ae89f AS build
RUN apk add --no-cache musl-dev build-base cmake perl
WORKDIR /src
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates/graphpg/Cargo.toml crates/graphpg/Cargo.toml
# Compile the dependencies against empty stand-in sources first. This layer only changes when a
# Cargo manifest or the lockfile does, so it is reused (including from the CI layer cache)
# across source-only changes, which is what makes rebuilds fast.
RUN mkdir -p crates/graphpg/src \
    && echo 'fn main() {}' > crates/graphpg/src/main.rs && touch crates/graphpg/src/lib.rs \
    && cargo build --release --locked --bin graphpg --features otel \
    && rm -rf crates/graphpg/src
COPY crates ./crates
# The real sources must look newer than the stand-ins, or cargo would reuse the stand-in build
RUN touch crates/graphpg/src/main.rs crates/graphpg/src/lib.rs \
    && cargo build --release --locked --bin graphpg --features otel \
    && cp target/release/graphpg /graphpg

# --- Runtime: distroless static --------------------------------------------------------
# No shell, no package manager, no libc; CA certificates and a non-root user only.
FROM gcr.io/distroless/static-debian12:nonroot@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab

ARG VERSION=dev
ARG REVISION=unknown
LABEL org.opencontainers.image.title="graphpg" \
      org.opencontainers.image.description="GraphQL for PostgreSQL, generated from the database catalogs" \
      org.opencontainers.image.source="https://github.com/mihailtd/graphpg" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"

COPY --from=build /graphpg /usr/local/bin/graphpg
WORKDIR /app
USER 65532:65532
ENV PORT=4000
EXPOSE 4000
ENTRYPOINT ["/usr/local/bin/graphpg"]
