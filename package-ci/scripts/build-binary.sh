#!/bin/bash
# Build binary .deb packages for a specific distro×arch
#
# Runs dpkg-buildpackage inside a podman container targeting the given distro.
# The container provides isolation; sbuild is not used (it requires nested user
# namespaces which don't work in rootless podman).
#
# Build environments are cached as podman images to avoid re-installing deps
# on every build. Cache is keyed on distro×arch×rust_version; invalidated by
# touching $CACHE_DIR/rebuild-$DISTRO-$ARCH or passing REBUILD_CACHE=1.
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
CACHE_IMAGE="ci-deps:${DISTRO}-${ARCH}-rust${RUST_VERSION}"

mkdir -p "$RESULT_DIR"

cleanup() {
    podman rm -f "$CONTAINER" 2>/dev/null || true
}
trap cleanup EXIT

echo "=== Building binary: $DISTRO $ARCH (commit ${COMMIT:0:12}) ==="

# Select base container image per distro
case "$DISTRO" in
    unstable) BASE_IMAGE="debian:unstable-slim" ;;
    forky)    BASE_IMAGE="debian:forky-slim" ;;
    trixie)   BASE_IMAGE="debian:trixie-slim" ;;
    plucky)   BASE_IMAGE="ubuntu:plucky" ;;
    questing) BASE_IMAGE="ubuntu:questing" ;;
    *) echo "ERROR: unknown distro $DISTRO"; exit 1 ;;
esac

# Cross-compilation: ppc64el builds run on amd64 host
CROSS_BUILD_DEP_ARCH=""
CROSS_DPKG_ARCH=""
if [ "$ARCH" = "ppc64el" ]; then
    CROSS_BUILD_DEP_ARCH="--host-architecture ppc64el"
    CROSS_DPKG_ARCH="-a ppc64el"
fi

# Find the .dsc file in the source directory
DSC_FILE=$(find "$SOURCE_DIR" -name "*.dsc" | head -1)
if [ -z "$DSC_FILE" ]; then
    echo "ERROR: no .dsc file found in $SOURCE_DIR"
    exit 1
fi
DSC_BASENAME=$(basename "$DSC_FILE")

# ---------------------------------------------------------------------------
# Cached build environment
# ---------------------------------------------------------------------------
#
# First build: install all deps, commit the container as $CACHE_IMAGE.
# Subsequent builds: start from $CACHE_IMAGE, skip straight to dpkg-buildpackage.
# The cache includes: build-essential, build-deps, cross-compilers, rustup.

REBUILD_CACHE="${REBUILD_CACHE:-0}"
REBUILD_MARKER="$CACHE_DIR/rebuild-$DISTRO-$ARCH"
if [ -f "$REBUILD_MARKER" ]; then
    REBUILD_CACHE=1
    rm -f "$REBUILD_MARKER"
fi

need_cache_build() {
    [ "$REBUILD_CACHE" = "1" ] && return 0
    ! podman image exists "$CACHE_IMAGE" 2>/dev/null
}

if need_cache_build; then
    echo "--- Building cached environment: $CACHE_IMAGE ---"

    BUILD_CONTAINER="ci-cache-build-${DISTRO}-${ARCH}-$$"
    podman rm -f "$BUILD_CONTAINER" 2>/dev/null || true

    podman run --name "$BUILD_CONTAINER" \
        --detach --init \
        --volume "$SOURCE_DIR:/source:ro" \
        --tmpfs /tmp:exec \
        "$BASE_IMAGE" sleep infinity

    crun() {
        podman exec "$BUILD_CONTAINER" bash -euxc "$*"
    }

    # Cross-compilation setup: add foreign arch before first apt-get update
    if [ "$ARCH" = "ppc64el" ]; then
        crun 'dpkg --add-architecture ppc64el'
    fi

    # Install essential build tools
    crun '
        apt-get update
        DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
            build-essential ca-certificates curl devscripts dpkg-dev
    '

    # Install cross-compilation tools
    if [ "$ARCH" = "ppc64el" ]; then
        crun '
            DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
                qemu-user-static binfmt-support gcc-powerpc64le-linux-gnu
        '
    fi

    # Extract source package and install build-deps (this installs distro Rust)
    crun "
        mkdir -p /build
        cp /source/* /build/
        cd /build
        dpkg-source -x ${DSC_BASENAME} src
        DEBIAN_FRONTEND=noninteractive apt-get build-dep -y ${CROSS_BUILD_DEP_ARCH} ./src
        rm -rf /build
    "

    # If distro Rust is too old (needs 1.85+ for edition2024), install via rustup
    # and replace the distro cargo/rustc with symlinks to the rustup-managed versions.
    crun "
        if ! rustc --version 2>/dev/null | grep -qE '1\\.(8[5-9]|9[0-9])'; then
            curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | \
                sh -s -- --default-toolchain ${RUST_VERSION} --profile minimal -y

            # Make rustup's cargo/rustc the system default so dpkg-buildpackage finds them
            ln -sf /root/.cargo/bin/cargo /usr/bin/cargo
            ln -sf /root/.cargo/bin/rustc /usr/bin/rustc

            # Replace Ubuntu's /usr/share/cargo/bin/cargo wrapper with a shim that
            # delegates to rustup cargo and handles prepare-debian as a no-op
            if [ -f /usr/share/cargo/bin/cargo ]; then
                printf '#!/bin/sh\n[ \"\\\$1\" = \"prepare-debian\" ] && exit 0\nRUSTUP_HOME=/root/.rustup exec /usr/bin/cargo \"\\\$@\"\n' \
                    > /usr/share/cargo/bin/cargo
                chmod +x /usr/share/cargo/bin/cargo
            fi
        fi
    "

    # Clean apt caches to keep the image small
    crun 'apt-get clean && rm -rf /var/lib/apt/lists/*'

    # Commit as cached image
    podman commit "$BUILD_CONTAINER" "$CACHE_IMAGE"
    podman rm -f "$BUILD_CONTAINER"

    echo "--- Cached environment ready: $CACHE_IMAGE ---"
fi

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

podman run --name "$CONTAINER" \
    --detach --init \
    --security-opt seccomp=unconfined \
    --security-opt apparmor=unconfined \
    --device /dev/fuse \
    --cap-add SYS_ADMIN \
    --volume "$SOURCE_DIR:/source:ro" \
    --volume "$RESULT_DIR:/result:rw" \
    --tmpfs /tmp:exec \
    "$CACHE_IMAGE" sleep infinity

run() {
    podman exec "$CONTAINER" bash -euxc "$*"
}

# Extract source and install any new build-deps not in the cached image
run "
    mkdir -p /build
    cp /source/* /build/
    cd /build
    dpkg-source -x ${DSC_BASENAME} src
    apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get build-dep -y ${CROSS_BUILD_DEP_ARCH} ./src
"

# Build
run "
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
