FROM --platform=$BUILDPLATFORM node:22-alpine AS frontend
WORKDIR /src/web
COPY web .
RUN yarn install --ignore-optional
RUN yarn build

FROM --platform=$BUILDPLATFORM rust:1.98-trixie AS chef
USER root
ENV CARGO_PROFILE_RELEASE_LTO=true
RUN cargo install cargo-chef
WORKDIR /app

FROM --platform=$BUILDPLATFORM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM --platform=$BUILDPLATFORM chef AS builder

ARG TARGETPLATFORM
RUN case "${TARGETPLATFORM}" in \
      "linux/arm64") echo "aarch64-unknown-linux-gnu" > /target.txt && echo "-C linker=aarch64-linux-gnu-gcc" > /flags.txt ;; \
      "linux/amd64") echo "x86_64-unknown-linux-gnu" > /target.txt && echo "-C linker=x86_64-linux-gnu-gcc" > /flags.txt ;; \
      *) exit 1 ;; \
    esac
RUN export DEBIAN_FRONTEND=noninteractive && \
    apt-get update && \
    apt-get install -yq build-essential g++-aarch64-linux-gnu binutils-aarch64-linux-gnu
RUN rustup target add "$(cat /target.txt)"

COPY --from=planner /app/recipe.json recipe.json
RUN RUSTFLAGS="$(cat /flags.txt)" cargo chef cook --target "$(cat /target.txt)" --release --recipe-path recipe.json
COPY . .
COPY --from=frontend /src/web/dist web/dist
RUN RUSTFLAGS="$(cat /flags.txt)" cargo build --target "$(cat /target.txt)" --release
RUN mv "./target/$(cat /target.txt)/release" "/output"

# Same Debian release as the builder so the glibc versions match
FROM debian:trixie-slim AS runtime
# curl is used as a fallback to look up chatrooms when Kick's website blocks the built in HTTP client
RUN export DEBIAN_FRONTEND=noninteractive && \
    apt-get update && \
    apt-get install -yq --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*
RUN useradd rustlog && mkdir /logs && chown rustlog: /logs
COPY --from=builder /output/rustlog-kick /usr/local/bin/
USER rustlog
CMD ["/usr/local/bin/rustlog-kick"]
