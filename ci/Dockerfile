# syntax=docker/dockerfile:1
#
# 自建 CI 镜像：Ubuntu 24.04 + Rust stable/1.85 + Node 22。
# 内部包代理地址不写入仓库：构建时用
#   --build-arg PKGS_BASE=https://<内部代理根>
# 注入；留空则走官方源。

FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive
ARG RUST_STABLE=stable
ARG RUST_MSRV=1.85
ARG NODE_VERSION=22.23.4
ARG PKGS_BASE=

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential ca-certificates curl git libssl-dev pkg-config xz-utils \
 && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain "${RUST_STABLE}" \
          --component rustfmt --component clippy \
 && rustup toolchain install "${RUST_MSRV}" --profile minimal \
 && rustup run "${RUST_STABLE}" rustc --version \
 && rustup run "${RUST_MSRV}" rustc --version

RUN curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
      | tar -xJ -C /usr/local --strip-components=1 --no-same-owner \
 && node --version && npm --version

# 可选：把 Cargo / npm 指向内部包代理。只影响构建产物，不含任何地址常量。
RUN if [ -n "${PKGS_BASE}" ]; then \
      mkdir -p "${CARGO_HOME}" \
      && printf '[source.crates-io]\nreplace-with = "internal"\n\n[source.internal]\nregistry = "sparse+%s/cargo/"\n' "${PKGS_BASE}" > "${CARGO_HOME}/config.toml" \
      && printf 'registry=%s/npm/\n' "${PKGS_BASE}" > /root/.npmrc; \
    fi

# 非 root 运行：部分测试依赖目录权限生效（如 chmod 0500 后写入失败）。
RUN mkdir -p "${CARGO_HOME}/registry" /home/ubuntu/.npm \
 && chown -R ubuntu:ubuntu "${RUSTUP_HOME}" "${CARGO_HOME}" /home/ubuntu

USER ubuntu
ENV HOME=/home/ubuntu

WORKDIR /work
