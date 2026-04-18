FROM fedora:45
LABEL maintainer="jdx"

# Install packaging dependencies. `cargo vendor` is built in to the
# cargo shipped with Fedora, so we don't need a separate tool.
RUN dnf update -y && \
    dnf install -y \
        rpm-build \
        rpmdevtools \
        python3-pip \
        git \
        rust \
        cargo \
        gcc \
        openssl-devel \
        pkgconf-pkg-config \
        tar \
        gzip && \
    dnf clean all

RUN pip3 install copr-cli

RUN rpmdev-setuptree
