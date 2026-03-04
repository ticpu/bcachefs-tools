#!/bin/bash
# Build binary .deb packages for a specific distro×arch
#
# Runs sbuild inside a podman container (debian:trixie-slim).
# sbuild creates an mmdebstrap chroot for the target distro.
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
IMAGE="debian:trixie-slim"

# Per-distro-arch apt cache to avoid lock contention when multiple builds run concurrently
APT_CACHE_DIR="$CACHE_DIR/apt-$DISTRO-$ARCH"
mkdir -p "$RESULT_DIR" "$APT_CACHE_DIR"

cleanup() {
    podman rm -f "$CONTAINER" 2>/dev/null || true
}
trap cleanup EXIT

echo "=== Building binary: $DISTRO $ARCH (commit ${COMMIT:0:12}) ==="

# Determine if this is a cross-build
BUILD_ARCH="$ARCH"
HOST_ARCH="$ARCH"
if [ "$ARCH" = "ppc64el" ]; then
    BUILD_ARCH="amd64"
fi

# Determine if Ubuntu
is_ubuntu() {
    case "$1" in
        plucky|questing) return 0 ;;
        *) return 1 ;;
    esac
}

# Find the .dsc file in the source directory
DSC_FILE=$(find "$SOURCE_DIR" -name "*.dsc" | head -1)
if [ -z "$DSC_FILE" ]; then
    echo "ERROR: no .dsc file found in $SOURCE_DIR"
    exit 1
fi

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

# Install build tools
run '
    apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        debian-keyring devscripts mmdebstrap sbuild sudo tar uidmap xz-utils
    # sbuild unshare mode needs subuid/subgid mappings for root inside the container
    echo "root:100000:65536" >> /etc/subuid
    echo "root:100000:65536" >> /etc/subgid
'

# Set up sbuild configuration
DSC_BASENAME=$(basename "$DSC_FILE")

# Build the sbuildrc
SBUILDRC="
\$verbose = 1;
\$build_dir = '/build';
\$distribution = '${DISTRO}';
\$build_arch = '${BUILD_ARCH}';
\$host_arch = '${HOST_ARCH}';
\$chroot_mode = 'unshare';
\$run_lintian = 0;
\$autopkgtest_root_args = '';
\$external_commands = {};
"

# Add mirror configuration based on distro
if is_ubuntu "$DISTRO"; then
    SBUILDRC+="
my \$debootstrap_mirror = 'http://archive.ubuntu.com/ubuntu';
my \$mmdebstrap_extra_args = [
    '--components=main,universe',
    '--variant=buildd',
];
"
else
    SBUILDRC+="
my \$debootstrap_mirror = 'http://deb.debian.org/debian';
my \$mmdebstrap_extra_args = [
    '--components=main',
    '--variant=buildd',
];
"
fi

# Add rustup chroot-setup-command for distros with old Rust
# Plucky ships Rust 1.84 which can't build edition2024
SBUILDRC+="
my \$chroot_setup_commands = [
    'apt-get update',
    'DEBIAN_FRONTEND=noninteractive apt-get install -y ca-certificates curl',
    'if ! rustc --version 2>/dev/null | grep -qE \"1\\.(8[5-9]|9[0-9])\"; then curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- --default-toolchain ${RUST_VERSION} --profile minimal -y; fi',
];
"

podman exec "$CONTAINER" bash -c "cat > /root/.sbuildrc << 'SBUILDRC_EOF'
${SBUILDRC}
SBUILDRC_EOF"

# Cross-compilation setup
if [ "$ARCH" = "ppc64el" ]; then
    run '
        DEBIAN_FRONTEND=noninteractive apt-get install -y \
            qemu-user-static binfmt-support
    '
fi

# Run sbuild
run "
    mkdir -p /build
    cp /source/* /build/ 2>/dev/null || true
    cd /build
    sbuild --verbose --arch-any --arch-all /build/${DSC_BASENAME}
"

# Copy results
run '
    find /build -name "*.deb" -exec cp {} /result/ \;
    find /build -name "*.ddeb" -exec cp {} /result/ \;
    find /build -name "*.changes" -exec cp {} /result/ \;
    find /build -name "*.buildinfo" -exec cp {} /result/ \;
'

echo "=== Binary build complete: $DISTRO $ARCH ==="
ls -la "$RESULT_DIR/"
