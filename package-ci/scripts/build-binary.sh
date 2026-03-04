#!/bin/bash
# Build binary .deb packages for a specific distro×arch
#
# Runs dpkg-buildpackage inside a podman container targeting the given distro.
# The container provides isolation; sbuild is not used (it requires nested user
# namespaces which don't work in rootless podman).
#
# Usage: build-binary.sh DISTRO ARCH COMMIT SOURCE_DIR RESULT_DIR RUST_VERSION
#
# DISTRO:     unstable|forky|trixie|questing|plucky
# ARCH:       amd64|ppc64el|arm64
# SOURCE_DIR: directory containing .dsc from source build
# RESULT_DIR: where to write output .deb files

set -euo pipefail

DISTRO="$1"
ARCH="$2"
COMMIT="$3"
SOURCE_DIR="$4"
RESULT_DIR="$5"
RUST_VERSION="$6"

CACHE_DIR="${CACHE_DIR:-/home/aptbcachefsorg/package-ci/cache}"
CONTAINER="ci-binary-${DISTRO}-${ARCH}-$$"

# Per-distro-arch apt cache to avoid lock contention when multiple builds run concurrently
APT_CACHE_DIR="$CACHE_DIR/apt-$DISTRO-$ARCH"
mkdir -p "$RESULT_DIR" "$APT_CACHE_DIR"

cleanup() {
    podman rm -f "$CONTAINER" 2>/dev/null || true
}
trap cleanup EXIT

echo "=== Building binary: $DISTRO $ARCH (commit ${COMMIT:0:12}) ==="

# Select container image per distro
case "$DISTRO" in
    unstable) IMAGE="debian:unstable-slim" ;;
    forky)    IMAGE="debian:forky-slim" ;;
    trixie)   IMAGE="debian:trixie-slim" ;;
    plucky)   IMAGE="ubuntu:plucky" ;;
    questing) IMAGE="ubuntu:questing" ;;
    *) echo "ERROR: unknown distro $DISTRO"; exit 1 ;;
esac

# Cross-compilation: ppc64el builds run on amd64 host
CROSS_BUILD_DEP_ARCH=""
CROSS_DPKG_ARCH=""
if [ "$ARCH" = "ppc64el" ]; then
    CROSS_BUILD_DEP_ARCH="--host-arch ppc64el"
    CROSS_DPKG_ARCH="-a ppc64el"
fi

# Find the .dsc file in the source directory
DSC_FILE=$(find "$SOURCE_DIR" -name "*.dsc" | head -1)
if [ -z "$DSC_FILE" ]; then
    echo "ERROR: no .dsc file found in $SOURCE_DIR"
    exit 1
fi
DSC_BASENAME=$(basename "$DSC_FILE")

podman run --name "$CONTAINER" \
    --detach --init \
    --security-opt seccomp=unconfined \
    --security-opt apparmor=unconfined \
    --device /dev/fuse \
    --cap-add SYS_ADMIN \
    --volume "$SOURCE_DIR:/source:ro" \
    --volume "$RESULT_DIR:/result:rw" \
    --volume "$APT_CACHE_DIR:/var/cache/apt:rw" \
    --tmpfs /tmp:exec \
    "$IMAGE" sleep infinity

run() {
    podman exec "$CONTAINER" bash -euxc "$*"
}

# Clear stale apt locks that may be left by crashed previous containers.
# Safe because each distro×arch gets its own cache dir and builds don't overlap.
run '
    rm -f /var/cache/apt/archives/lock
    rm -f /var/lib/apt/lists/lock
    rm -f /var/lib/dpkg/lock
    rm -f /var/lib/dpkg/lock-frontend
'

# Cross-compilation setup (ppc64el on amd64)
if [ "$ARCH" = "ppc64el" ]; then
    run '
        DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
            qemu-user-static binfmt-support
        dpkg --add-architecture ppc64el
        apt-get update
    '
fi

# Install essential build tools (no Rust yet - build-dep will pull the right version)
run '
    apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl devscripts dpkg-dev
'

# Extract source package and install build-deps (this installs distro Rust)
run "
    mkdir -p /build
    cp /source/* /build/
    cd /build
    dpkg-source -x ${DSC_BASENAME} src
    DEBIAN_FRONTEND=noninteractive apt-get build-dep -y ${CROSS_BUILD_DEP_ARCH} ./src
"

# If distro Rust is too old (needs 1.85+ for edition2024), install via rustup.
# debian/rules hardcodes CARGO=/usr/share/cargo/bin/cargo, so we also replace
# that path with a shim. Only do this if the path exists (Ubuntu-specific wrapper).
run "
    if ! rustc --version 2>/dev/null | grep -qE '1\\.(8[5-9]|9[0-9])'; then
        curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | \
            sh -s -- --default-toolchain ${RUST_VERSION} --profile minimal -y
        # Replace system cargo wrapper (Ubuntu-specific path) with rustup shim
        if [ -f /usr/share/cargo/bin/cargo ]; then
            printf '#!/bin/sh\nexec /root/.cargo/bin/cargo \"\$@\"\n' \
                > /usr/share/cargo/bin/cargo
            chmod +x /usr/share/cargo/bin/cargo
        fi
        ln -sf /root/.cargo/bin/rustc /usr/bin/rustc
    fi
"

# Build
run "
    export PATH=\"\${HOME}/.cargo/bin:\${PATH}\"
    cd /build/src
    dpkg-buildpackage -us -uc -b ${CROSS_DPKG_ARCH}
"

# Copy results
run '
    find /build -maxdepth 1 -name "*.deb" -exec cp {} /result/ \;
    find /build -maxdepth 1 -name "*.ddeb" -exec cp {} /result/ \;
    find /build -maxdepth 1 -name "*.changes" -exec cp {} /result/ \;
    find /build -maxdepth 1 -name "*.buildinfo" -exec cp {} /result/ \;
'

echo "=== Binary build complete: $DISTRO $ARCH ==="
ls -la "$RESULT_DIR/"
