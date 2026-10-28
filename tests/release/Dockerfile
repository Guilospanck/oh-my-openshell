# For tests/release/run.sh: one image that is both the `act` driver (nektos/act, talking to the
# Docker daemon through its socket) and the job container act runs the workflow's ubuntu-24.04
# jobs in. As the job container it carries what GitHub's ubuntu-24.04 runner image has and the
# workflow relies on (git, curl, a C toolchain, rustup with cargo, Node 20 for the JS actions), and
# a recording `gh`, so the publish job runs without creating a real release.
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
    && apt-get install --yes --no-install-recommends bash build-essential ca-certificates curl git jq xz-utils zstd \
    && rm -rf /var/lib/apt/lists/*
ARG ACT_VERSION=v0.2.89
ARG NODE_VERSION=v20.20.2
RUN case "$(uname -m)" in aarch64) a=arm64; n=arm64 ;; *) a=x86_64; n=x64 ;; esac \
    && curl -fsSL "https://github.com/nektos/act/releases/download/${ACT_VERSION}/act_Linux_${a}.tar.gz" | tar -xz -C /usr/local/bin act \
    && curl -fsSL "https://nodejs.org/dist/${NODE_VERSION}/node-${NODE_VERSION}-linux-${n}.tar.xz" | tar -xJ -C /opt \
    && ln -s "/opt/node-${NODE_VERSION}-linux-${n}/bin/node" /usr/local/bin/node
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path \
    && chmod -R a+rwX /opt/rustup /opt/cargo
ENV PATH=/opt/cargo/bin:$PATH
COPY gh /usr/local/bin/gh
