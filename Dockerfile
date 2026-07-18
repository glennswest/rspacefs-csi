# Multi-stage build for the rspacefs-csi driver.
# Build host convention follows rspacefs: Fedora + system protoc.
ARG FEDORA_VERSION=43

FROM registry.fedoraproject.org/fedora:${FEDORA_VERSION} AS build
RUN dnf -y install cargo rust protobuf-compiler && dnf clean all
WORKDIR /src
# Cache dependencies first.
COPY Cargo.toml Cargo.lock ./
COPY crates/csi-proto/Cargo.toml crates/csi-proto/Cargo.toml
COPY crates/rspacefs-csi/Cargo.toml crates/rspacefs-csi/Cargo.toml
COPY proto proto
COPY crates crates
RUN cargo build --workspace --release

FROM registry.fedoraproject.org/fedora:${FEDORA_VERSION}
# fuse3/fusermount3 are needed by the node plugin to tear down FUSE mounts;
# rspacefs-mount itself is delivered by the rspacefs image/package.
RUN dnf -y install fuse3 && dnf clean all
COPY --from=build /src/target/release/rspacefs-csi /usr/local/bin/rspacefs-csi
ENTRYPOINT ["/usr/local/bin/rspacefs-csi"]
