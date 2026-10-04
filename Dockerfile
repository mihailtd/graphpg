# syntax=docker/dockerfile:1

# --- Build: UI assets ------------------------------------------------------------------
# Base images are pinned by digest; Dependabot proposes updates.
FROM node:26-alpine@sha256:b341ca66519d9a1c25d4e41f254ffb6fe403fc0f9054c62b863f0660dcc1c199 AS ui
WORKDIR /src
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
# pnpm at the version package.json pins (Node 26 images no longer ship Corepack)
RUN npm install --global "pnpm@$(node -p "require('./package.json').packageManager.split('@')[1]")"
# Dependencies first, so this layer is reused until a manifest or the lockfile changes
COPY packages/graphiql-ui/package.json packages/graphiql-ui/package.json
RUN pnpm install --frozen-lockfile --filter @graphpg/graphiql-ui
COPY packages/graphiql-ui packages/graphiql-ui
RUN pnpm --filter @graphpg/graphiql-ui run build

# --- Build: a fully static musl binary -------------------------------------------------
# Base images are pinned by digest; Dependabot proposes updates.
FROM rust:1.98.1-alpine@sha256:7cc1c22d77d9432f7fe012a70e6d3e555af54c2a6832700ed7d553f1769ae89f AS build
RUN apk add --no-cache musl-dev build-base cmake perl
WORKDIR /src
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates/graphpg/Cargo.toml crates/graphpg/Cargo.toml
COPY crates/graphpg-ui/Cargo.toml crates/graphpg-ui/Cargo.toml
# Compile the dependencies against empty stand-in sources first. This layer only changes when a
# Cargo manifest or the lockfile does, so it is reused (including from the CI layer cache)
# across source-only changes, which is what makes rebuilds fast.
RUN mkdir -p crates/graphpg/src crates/graphpg-ui/src \
    && echo 'fn main() {}' > crates/graphpg/src/main.rs && touch crates/graphpg/src/lib.rs \
    && touch crates/graphpg-ui/src/lib.rs \
    && cargo build --release --locked --bin graphpg --features otel \
    && rm -rf crates/graphpg/src crates/graphpg-ui/src
COPY crates ./crates
COPY --from=ui /src/crates/graphpg-ui/assets crates/graphpg-ui/assets
ENV GRAPHPG_REQUIRE_UI=1
# The real sources must look newer than the stand-ins, or cargo would reuse the stand-in build
RUN touch crates/graphpg/src/main.rs crates/graphpg/src/lib.rs crates/graphpg-ui/src/lib.rs \
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
