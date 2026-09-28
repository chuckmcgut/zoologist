# syntax=docker/dockerfile:1
#
# Zoologist image (plan Step 11.3).
#
#   docker compose build                      # default target: runtime (distroless, pure-Rust decoder)
#   docker build --target runtime-ffmpeg .    # only if [video] decoder = "ffmpeg"
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
COPY static /static
COPY --from=build --chown=65532:65532 /data-empty /data
# Run from / so that relative paths in a config written for a checkout (models/…, data, static)
# land on the mounts: /models, /data, and the page files at /static.
WORKDIR /
# glibc keeps a memory pool per thread and rarely hands freed memory back; with many threads
# (decoders, detector workers, recorder) that looks like a leak. Two pools are plenty here.
ENV MALLOC_ARENA_MAX=2
EXPOSE 8090
ENTRYPOINT ["/usr/local/bin/zoologist"]
CMD ["run", "--config", "/config/zoologist.toml"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s CMD ["/usr/local/bin/zoologist", "healthcheck"]

# Only if the pure-Rust decoder cannot handle a camera (plan Step 0.4): adds ffmpeg.
FROM debian:bookworm-slim AS runtime-ffmpeg
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /zoologist /usr/local/bin/zoologist
COPY static /static
COPY --from=build --chown=65532:65532 /data-empty /data
# Run from / so that relative paths in a config written for a checkout (models/…, data, static)
# land on the mounts: /models, /data, and the page files at /static.
WORKDIR /
USER 65532
# glibc keeps a memory pool per thread and rarely hands freed memory back; with many threads
# (decoders, detector workers, recorder) that looks like a leak. Two pools are plenty here.
ENV MALLOC_ARENA_MAX=2
EXPOSE 8090
ENTRYPOINT ["/usr/local/bin/zoologist"]
CMD ["run", "--config", "/config/zoologist.toml"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s CMD ["/usr/local/bin/zoologist", "healthcheck"]

# The last stage is what a plain `docker build .` produces.
FROM runtime
