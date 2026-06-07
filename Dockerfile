# syntax=docker/dockerfile:1

FROM rust:1-trixie AS build

ARG GROK_VERSION=v20.3.3
ARG GIGATIFF_BUILD_GROK=1
ARG GIGATIFF_SERVER_FEATURES=jpeg2000-grok-ffi

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
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

RUN if [ "${GIGATIFF_BUILD_GROK}" = "1" ]; then \
    git clone --depth 1 --branch ${GROK_VERSION} --recursive https://github.com/GrokImageCompression/grok.git /tmp/grok \
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
    && rm -rf /tmp/grok /tmp/grok-build; \
  fi

WORKDIR /app
COPY . .
RUN if [ -n "${GIGATIFF_SERVER_FEATURES}" ]; then \
      cargo build --release --locked -p gigatiff-server --bin gigatiff-server --features "${GIGATIFF_SERVER_FEATURES}"; \
    else \
      cargo build --release --locked -p gigatiff-server --bin gigatiff-server; \
    fi

FROM debian:trixie-slim

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
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
