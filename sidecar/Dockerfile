FROM rust:1-bookworm AS builder

WORKDIR /app

# Needed for reqwest's default-tls (openssl) and general builds.
RUN apt-get update \
  && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
  && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations

RUN cargo build --release --bin nocy-wallet-feed

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates curl libssl3 \
  && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home --shell /usr/sbin/nologin appuser

COPY --from=builder /app/target/release/nocy-wallet-feed /usr/local/bin/nocy-wallet-feed

USER appuser

EXPOSE 8080

ENTRYPOINT ["nocy-wallet-feed"]

