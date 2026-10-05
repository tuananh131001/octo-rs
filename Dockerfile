# Octo's image. The same contract the C# image (up to 2026.10.03.2) had: port 8080
# (ASPNETCORE_URLS), /app/config for settings and state, /music for the library,
# /app/downloads created, and the app itself as PID 1 so SIGTERM reaches it and it
# drains requests and workers within its 10 s budget.
#
#   docker build -t octo:dev .
#   docker build --build-arg OCTO_VERSION=2026.11.01.1 -t octo:dev .   # instead of VERSION

ARG RUST_VERSION=1.99

# ---- Toolchain + cargo-chef (cached until the Rust version changes) ----
FROM rust:${RUST_VERSION}-bookworm AS chef
# The machine that builds this has 4 cores and 7 GB of RAM; two jobs keep the
# LTO link and the heavier crates (lofty, image, cosmic-text) within it.
ENV CARGO_BUILD_JOBS=2 \
    CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0
RUN cargo install cargo-chef --locked
WORKDIR /src

# ---- Dependency recipe: changes only when a Cargo.toml or Cargo.lock does ----
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo chef prepare --recipe-path recipe.json

# ---- Build ----
FROM chef AS build
COPY --from=planner /src/recipe.json recipe.json
# Every third-party crate, compiled once and cached in this layer.
RUN cargo chef cook --release --locked --package octo --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
# Sources, plus what the binary compiles in: the list-cover design (JSON, Inter fonts,
# painted backgrounds) under crates/octo-media/assets and the built-in appsettings under
# crates/octo-core/assets.
COPY crates crates

# The release the dashboard and User-Agent show: the VERSION file, compiled in by
# octo-core's build.rs, so a bump rebuilds octo-core and everything above it.
# --build-arg OCTO_VERSION=... stamps another release without editing the file.
COPY VERSION ./
ARG OCTO_VERSION
RUN cargo build --release --locked --package octo --bin octo \
    && install -D -m 0755 target/release/octo /out/octo

# ---- Runtime ----
FROM debian:bookworm-slim
WORKDIR /app

# Continuous Subsonic Radio normalizes mixed FLAC/M4A sources into one stable
# MP3 response inside the core Octo process. This is a runtime dependency, not a
# Radio sidecar or service boundary.
# fpcalc (libchromaprint-tools) is the other half of download verification: it turns a
# finished download into the Chromaprint fingerprint AcoustID is asked about. Absent, the
# feature degrades to a no-op and logs once; it never fails a download.
# Generated list covers set names in Inter (inside the app); Noto CJK and Symbola draw the
# Chinese, Japanese, Korean and emoji names Inter has no letters for, DejaVu Arabic and Hebrew.
# ca-certificates: the .NET base image carried a root store; slim Debian does not, and every
# upstream (Deezer, MusicBrainz, Last.fm, GitHub, ...) is HTTPS.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates ffmpeg fonts-dejavu-core fonts-noto-cjk fonts-symbola libchromaprint-tools \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /app/downloads /app/config

# The binary looks for wwwroot/ and Assets/ beside itself (StaticRoots::from_env, and the
# cover service's logo lookup), as the C# app looked in its publish root.
COPY --from=build /out/octo /app/octo
COPY crates/octo/wwwroot /app/wwwroot
COPY crates/octo/Assets /app/Assets
# Inter is compiled into the binary; its licence ships beside it.
COPY crates/octo-media/assets/cover-design/Fonts/OFL.txt /app/licenses/Inter-OFL.txt

# No version label here: the published image gets org.opencontainers.image.version (and the
# rest) from docker.yml's metadata step, and the release itself is in the binary.
LABEL org.opencontainers.image.title="octo" \
      org.opencontainers.image.licenses="MIT"

EXPOSE 8080
ENV ASPNETCORE_URLS=http://+:8080

# Exec form: octo is PID 1 and receives SIGTERM from `docker stop` directly.
ENTRYPOINT ["/app/octo"]
