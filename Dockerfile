# syntax=docker/dockerfile:1

ARG RUST_IMAGE=rust:1.96.0-trixie
ARG DEBIAN_RUNTIME_IMAGE=debian:trixie-slim
FROM ${RUST_IMAGE} AS build

ARG GROK_REPOSITORY=https://github.com/GrokImageCompression/grok.git
ARG GROK_REF=06c96bfbc38c74d386ce27018002d3febfff8f0c
ARG GIGATIFF_BUILD_GROK=1
ARG GIGATIFF_SERVER_FEATURES=jpeg2000-grok-ffi
ARG GIGATIFF_BUILD_JOBS=2

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
    git clone --filter=blob:none --no-checkout "${GROK_REPOSITORY}" /tmp/grok \
    && git -C /tmp/grok fetch --depth 1 origin "${GROK_REF}" \
    && git -C /tmp/grok checkout --detach FETCH_HEAD \
    && git -C /tmp/grok submodule update --init --recursive --depth 1 \
    && cmake -S /tmp/grok -B /tmp/grok-build \
      -DCMAKE_BUILD_TYPE=Release \
      -DCMAKE_INSTALL_PREFIX=/usr/local \
      -DBUILD_SHARED_LIBS=ON \
      -DBUILD_TESTING=OFF \
      -DGRK_BUILD_CODEC=ON \
      -DGRK_BUILD_DOC=OFF \
      -DGRK_BUILD_JPEG=OFF \
    && cmake --build /tmp/grok-build --parallel "${GIGATIFF_BUILD_JOBS}" \
    && cmake --install /tmp/grok-build \
    && ldconfig \
    && rm -rf /tmp/grok /tmp/grok-build; \
  fi

WORKDIR /app
COPY . .
RUN if [ -n "${GIGATIFF_SERVER_FEATURES}" ]; then \
      cargo build --release --locked --jobs "${GIGATIFF_BUILD_JOBS}" -p gigatiff-server --bin gigatiff-server --features "${GIGATIFF_SERVER_FEATURES}"; \
    else \
      cargo build --release --locked --jobs "${GIGATIFF_BUILD_JOBS}" -p gigatiff-server --bin gigatiff-server; \
    fi

FROM ${DEBIAN_RUNTIME_IMAGE}

LABEL org.opencontainers.image.title="GigaTIFF Server" \
      org.opencontainers.image.description="IIIF-compatible TIFF/BigTIFF/JPEG2000 image server" \
      org.opencontainers.image.source="https://github.com/bezverec/gigatiff" \
      org.opencontainers.image.licenses="GPL-3.0-only"

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    libopenjp2-tools \
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
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8080/healthz >/dev/null || exit 1

ENTRYPOINT ["gigatiff-server"]
CMD ["--root", "/data", "--cache-dir", "/cache", "--cache-max-mb", "4096", "--max-concurrent-renders", "4", "--addr", "0.0.0.0:8080", "--enforce-read-only-root"]
