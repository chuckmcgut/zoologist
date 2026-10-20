# syntax=docker/dockerfile:1
#
# Zoologist image (plan Step 11.3).
#
#   docker compose build                      # default target: runtime (distroless, pure-Rust decoder)
#   docker build --target runtime-ffmpeg .    # plus a small ffmpeg: battery cameras' H.265 recordings
#   docker build --build-arg TARGET_CPU=x86-64-v3 .   # AVX2 build for the Ryzen 5700U (needs CPU type "host")
#   docker build --build-arg STRIP=none .              # keep line tables for `perf` (image ~150 MB larger)
#
# Models are not baked in: mount them at /models (see docs/MODELS.md).

FROM rust:1-bookworm AS build
ARG TARGET_CPU=""
# "debuginfo" drops the line tables (symbols stay, so backtraces still name functions).
ARG STRIP=debuginfo
WORKDIR /src
COPY . .
# The cache mounts keep downloaded crates and compiled dependencies between builds.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    if [ -n "$TARGET_CPU" ]; then export RUSTFLAGS="-C target-cpu=$TARGET_CPU"; fi; \
    CARGO_PROFILE_RELEASE_STRIP="$STRIP" cargo build --release --locked -p zoologist-server && \
    cp target/release/zoologist /zoologist
# An empty /data owned by the runtime user, so a fresh named volume is writable.
RUN mkdir -p /data-empty

# Default: distroless, no shell, no ffmpeg. Runs as uid 65532.
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
COPY --from=build /zoologist /usr/local/bin/zoologist
COPY --from=build --chown=65532:65532 /data-empty /data
# Run from / so that relative paths in a config written for a checkout (models/…, data) land on
# the mounts /models and /data. The dashboard is built into the program.
WORKDIR /
# glibc keeps a memory pool per thread and rarely hands freed memory back; with many threads
# (decoders, detector workers, recorder) that looks like a leak. Two pools are plenty here.
ENV MALLOC_ARENA_MAX=2
EXPOSE 8090
ENTRYPOINT ["/usr/local/bin/zoologist"]
CMD ["run", "--config", "/config/zoologist.toml"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s CMD ["/usr/local/bin/zoologist", "healthcheck"]

# A small ffmpeg with only what Zoologist asks of it: one frame of an MP4 (H.264 or H.265) as a
# JPEG, for sharp species views from the main recordings of Reolink battery cameras, and raw
# H.264 decoding on a pipe ([video] decoder = "ffmpeg"). LGPL build (no GPL or non-free parts);
# its source is the release archive below. It needs only glibc, which the distroless image has.
FROM debian:bookworm-slim AS ffmpeg
ARG FFMPEG_VERSION=7.1.1
ARG FFMPEG_SHA256=f117507dc501f2a6c11f9241d8d0c3213846cfad91764361af37befd6b6c523d
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl gcc make nasm libc6-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /ffmpeg
RUN curl -fsSL --retry 3 -o ffmpeg.tar.gz \
        "https://github.com/FFmpeg/FFmpeg/archive/refs/tags/n${FFMPEG_VERSION}.tar.gz" \
    && echo "${FFMPEG_SHA256}  ffmpeg.tar.gz" | sha256sum -c - \
    && tar xf ffmpeg.tar.gz --strip-components=1 \
    && ./configure --prefix=/opt/ffmpeg \
        --disable-everything --disable-autodetect --disable-doc --disable-debug \
        --disable-ffplay --disable-ffprobe --disable-avdevice --disable-network \
        --enable-protocol=file,pipe \
        --enable-demuxer=mov,h264,hevc --enable-parser=h264,hevc \
        --enable-decoder=h264,hevc \
        --enable-encoder=mjpeg,rawvideo --enable-muxer=image2pipe,rawvideo \
        --enable-filter=scale,format,null \
    && make -j"$(nproc)" ffmpeg \
    && strip ffmpeg && mkdir -p /opt/ffmpeg/bin && cp ffmpeg /opt/ffmpeg/bin/

# With ffmpeg: the default image plus the small ffmpeg above (a few MB).
FROM runtime AS runtime-ffmpeg
COPY --from=ffmpeg /opt/ffmpeg/bin/ffmpeg /usr/local/bin/ffmpeg

# The last stage is what a plain `docker build .` produces.
FROM runtime
