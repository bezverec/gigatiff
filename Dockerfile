# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS build

ARG GROK_VERSION=v20.3.3
ARG GIGATIFF_SERVER_FEATURES=server,jpeg2000-grok-ffi

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    clang \
    cmake \
    g++ \
    git \
    libclang-dev \
    liblcms2-dev \
    libtiff-dev \
    make \
    pkg-config \
  && rm -rf /var/lib/apt/lists/*

RUN git clone --depth 1 --branch ${GROK_VERSION} --recursive https://github.com/GrokImageCompression/grok.git /tmp/grok \
  && cmake -S /tmp/grok -B /tmp/grok-build \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX=/usr/local \
    -DBUILD_SHARED_LIBS=ON \
    -DBUILD_TESTING=OFF \
    -DGRK_BUILD_CODEC=ON \
    -DGRK_BUILD_DOC=OFF \
  && cmake --build /tmp/grok-build --parallel \
  && cmake --install /tmp/grok-build \
  && ldconfig \
  && rm -rf /tmp/grok /tmp/grok-build

WORKDIR /app
COPY . .
RUN cargo build --release --locked --bin gigatiff-server --no-default-features --features ${GIGATIFF_SERVER_FEATURES}

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    liblcms2-2 \
    libstdc++6 \
    libtiff6 \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /srv/gigatiff
COPY --from=build /usr/local /usr/local
COPY --from=build /app/target/release/gigatiff-server /usr/local/bin/gigatiff-server
RUN ldconfig

EXPOSE 8080
VOLUME ["/data", "/cache"]

ENTRYPOINT ["gigatiff-server"]
CMD ["--root", "/data", "--cache-dir", "/cache", "--cache-max-mb", "4096", "--max-concurrent-renders", "4", "--addr", "0.0.0.0:8080"]
