# syntax=docker/dockerfile:1

ARG RUST_IMAGE=rust:slim-bookworm
FROM --platform=$BUILDPLATFORM ${RUST_IMAGE} AS build

ARG ZIG_VERSION=0.16.0
ARG CARGO_ZIGBUILD_VERSION=0.23.4

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        binutils build-essential ca-certificates cmake git pkg-config python3-venv \
    && rm -rf /var/lib/apt/lists/*

ENV PATH="/opt/probe-build-tools/bin:${PATH}"
RUN python3 -m venv /opt/probe-build-tools \
    && pip install --no-cache-dir "ziglang==${ZIG_VERSION}" \
    && cargo install cargo-zigbuild --version "${CARGO_ZIGBUILD_VERSION}" --locked

WORKDIR /src
COPY . .

ARG TARGETOS
ARG TARGETARCH
ENV SQLX_OFFLINE=true

# Cross-compile on the builder's native architecture, without QEMU. Copy the
# result out of the cache mount so the export stage can access it.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=psa-probe-target-${TARGETARCH},target=/src/target,sharing=locked \
    set -eu; \
    case "${TARGETOS}/${TARGETARCH}" in \
        linux/amd64) target=x86_64-unknown-linux-musl ;; \
        linux/arm64) target=aarch64-unknown-linux-musl ;; \
        *) echo "Unsupported probe platform: ${TARGETOS}/${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    scripts/package-probe.sh "${target}"; \
    mkdir -p /out; \
    cp "target/${target}/release/psa_probe" /out/psa_probe

# Build-only artifact export, not a probe runtime container.
FROM scratch AS artifact
COPY --from=build /out/psa_probe /psa_probe
