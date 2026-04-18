FROM buildkite/hosted-agent-base:latest

ENV DEBIAN_FRONTEND=noninteractive
ENV MISE_EXPERIMENTAL=true
ENV MISE_YES=true

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    bash \
    build-essential \
    ca-certificates \
    curl \
    git \
    libssl-dev \
    parallel \
    pkg-config \
    xz-utils \
  && rm -rf /var/lib/apt/lists/*

RUN curl https://mise.run | sh
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile default --default-toolchain stable \
  && /root/.cargo/bin/rustup toolchain install 1.93.0 --profile default \
  && /root/.cargo/bin/rustup component add rustfmt clippy --toolchain stable

ENV PATH="/root/.cargo/bin:/root/.local/bin:/root/.local/share/mise/shims:${PATH}"
