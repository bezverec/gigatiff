# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS build

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    grokj2k-tools \
    liblcms2-dev \
    libtiff-dev \
    pkg-config \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .
RUN cargo build --release --locked --bin gigatiff-server --no-default-features --features server,jpeg2000-grok

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    grokj2k-tools \
    liblcms2-2 \
    libtiff6 \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /srv/gigatiff
COPY --from=build /app/target/release/gigatiff-server /usr/local/bin/gigatiff-server

EXPOSE 8080
VOLUME ["/data", "/cache"]

ENTRYPOINT ["gigatiff-server"]
CMD ["--root", "/data", "--cache-dir", "/cache", "--cache-max-mb", "4096", "--max-concurrent-renders", "4", "--addr", "0.0.0.0:8080"]
