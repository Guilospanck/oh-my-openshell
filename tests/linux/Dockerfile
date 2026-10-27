# Linux test environment with the build prerequisites that `just doctor` checks.
# Built and run by tests/linux/run.sh for each distro (BASE = ubuntu:24.04, fedora:41, archlinux:latest);
# not a sandbox image.
ARG BASE=ubuntu:24.04
FROM ${BASE}

ENV DEBIAN_FRONTEND=noninteractive
RUN . /etc/os-release \
    && case "$ID" in \
        ubuntu|debian) apt-get update \
            && apt-get install --yes --no-install-recommends bash build-essential ca-certificates curl git jq xz-utils \
            && rm -rf /var/lib/apt/lists/* ;; \
        fedora) dnf install -y bash gcc gcc-c++ make ca-certificates curl git jq xz tar gzip findutils which \
            && dnf clean all ;; \
        arch) pacman -Sy --noconfirm --needed base-devel bash ca-certificates curl git jq xz ;; \
        *) echo "unsupported distro $ID" >&2; exit 1 ;; \
    esac

# just, Zig 0.15 (libghostty-vt builds only with it) and gitleaks (the pre-commit hook), from releases.
RUN curl -fsSL https://just.systems/install.sh | bash -s -- --to /usr/local/bin
RUN arch="$(uname -m)" \
    && curl -fsSL "https://ziglang.org/download/0.15.2/zig-${arch}-linux-0.15.2.tar.xz" | tar -xJ -C /opt \
    && ln -s "/opt/zig-${arch}-linux-0.15.2/zig" /usr/local/bin/zig
RUN case "$(uname -m)" in aarch64) a=arm64 ;; *) a=x64 ;; esac \
    && v="$(curl -fsSL https://api.github.com/repos/gitleaks/gitleaks/releases/latest | jq -r .tag_name | tr -d v)" \
    && curl -fsSL "https://github.com/gitleaks/gitleaks/releases/download/v${v}/gitleaks_${v}_linux_${a}.tar.gz" | tar -xz -C /usr/local/bin gitleaks

ENV RUSTUP_HOME=/root/.rustup CARGO_HOME=/root/.cargo
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --component rustfmt,clippy
ENV PATH=/root/.cargo/bin:$PATH
WORKDIR /work
