FROM fedora:45
LABEL maintainer="jdx"

# Install packaging dependencies. `cargo vendor` is built in to the
# cargo shipped with Fedora, so we don't need a separate tool.
# `copr-cli` is installed from Fedora's own packages because
# Fedora 38+ enforces PEP 668 and blocks `pip install` into the
# system Python.
RUN dnf update -y && \
    dnf install -y \
        rpm-build \
        rpmdevtools \
        copr-cli \
        git \
        rust \
        cargo \
        gcc \
        openssl-devel \
        pkgconf-pkg-config \
        tar \
        gzip && \
    dnf clean all

RUN rpmdev-setuptree
