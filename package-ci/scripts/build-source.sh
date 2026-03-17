#!/bin/bash
# Build source-only Debian package for bcachefs-tools
#
# Runs inside a podman container (debian:trixie-slim).
# Produces: .dsc + .orig.tar.xz + .debian.tar.xz + .changes in $RESULT_DIR
#
# Usage: build-source.sh COMMIT GIT_REPO RESULT_DIR RUST_VERSION

set -euo pipefail

COMMIT="$1"
GIT_REPO="$2"
RESULT_DIR="$3"
RUST_VERSION="$4"

CACHE_DIR="${CACHE_DIR:-/home/aptbcachefsorg/package-ci/cache}"
CONTAINER="ci-source-$$"
IMAGE="debian:trixie-slim"

mkdir -p "$RESULT_DIR" "$CACHE_DIR/rustup" "$CACHE_DIR/cargo" "$CACHE_DIR/apt"

cleanup() {
    podman rm -f "$CONTAINER" 2>/dev/null || true
}
trap cleanup EXIT

echo "=== Building source package for $COMMIT ==="

# Clone the repo at the target commit into a temp dir
WORK_DIR=$(mktemp -d)
trap 'cleanup; rm -rf "$WORK_DIR"' EXIT

git clone --tags "$GIT_REPO" "$WORK_DIR/bcachefs-tools"
cd "$WORK_DIR/bcachefs-tools"
git checkout "$COMMIT"

# Determine version from git describe / .version, not debian/changelog
if git describe --tags --exact-match "$COMMIT" &>/dev/null; then
    # Tagged release: use the tag directly (strip leading 'v')
    RAW_VERSION=$(git describe --tags --exact-match "$COMMIT" | sed 's/^v//')
    NEW_VERSION="$RAW_VERSION"
else
    # Snapshot: base version from git describe or .version + snapshot suffix
    RAW_VERSION=$(git describe --tags --abbrev=0 2>/dev/null | sed 's/^v//' || cat .version 2>/dev/null | sed 's/^v//' || echo "0.0.0")
    SHORT_COMMIT=$(echo "$COMMIT" | head -c 12)
    SNAPSHOT_DATE=$(date -u +%Y%m%d%H%M%S)
    NEW_VERSION="${RAW_VERSION}~${SNAPSHOT_DATE}.gbp${SHORT_COMMIT}"
fi

cd "$WORK_DIR"

podman run --name "$CONTAINER" \
    --detach --init \
    --volume "$WORK_DIR/bcachefs-tools:/src:rw" \
    --volume "$CACHE_DIR/rustup:/root/.rustup:rw" \
    --volume "$CACHE_DIR/cargo:/root/.cargo:rw" \
    --volume "$CACHE_DIR/apt:/var/cache/apt:rw" \
    --tmpfs /tmp:exec \
    "$IMAGE" sleep infinity

run() {
    podman exec "$CONTAINER" bash -euxc "$*"
}

# Install build dependencies
run '
    apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates curl devscripts git git-buildpackage sbuild \
        gcc libc6-dev mmdebstrap patch tar uidmap xz-utils gnupg
'

# Install/update rustup (cached across builds)
run "
    if [ ! -f /root/.cargo/bin/rustup ]; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
            sh -s -- --default-toolchain $RUST_VERSION --profile minimal -y
    else
        export PATH=/root/.cargo/bin:\$PATH
        rustup default $RUST_VERSION
    fi
"

# Install cargo-vendor-filterer (cached via cargo)
run '
    export PATH=/root/.cargo/bin:$PATH
    if ! command -v cargo-vendor-filterer &>/dev/null; then
        cargo install --locked cargo-vendor-filterer
    fi
'

# Build source package (dpkg-buildpackage, not sbuild — no chroot needed for source)
run "
    export PATH=/root/.cargo/bin:\$PATH
    export DEBEMAIL='kent.overstreet@linux.dev'
    export DEBFULLNAME='Kent Overstreet'
    cd /src

    # Update changelog with correct version
    gbp dch --new-version='$NEW_VERSION' --since=HEAD~1 \
        --release --distribution=unstable --urgency=medium \
        --git-author || true

    # Build source-only package
    gbp buildpackage \
        --git-verbose \
        --git-ignore-branch \
        --git-ignore-new \
        --git-builder='dpkg-buildpackage -d -S -us -uc -nc'
"

# Collect results — gbp puts them in the export dir one level above /src
podman exec "$CONTAINER" bash -c "
    mkdir -p /src/result
    find /bcachefs-tools-deb-export-dir -name '*.dsc' -exec cp {} /src/result/ \\; 2>/dev/null || true
    find /bcachefs-tools-deb-export-dir -name '*.tar.*' -exec cp {} /src/result/ \\; 2>/dev/null || true
    find /bcachefs-tools-deb-export-dir -name '*.changes' -exec cp {} /src/result/ \\; 2>/dev/null || true
    find /bcachefs-tools-deb-export-dir -name '*.buildinfo' -exec cp {} /src/result/ \\; 2>/dev/null || true
    # Also check parent of /src
    find /src/.. -maxdepth 1 -name '*.dsc' -exec cp {} /src/result/ \\; 2>/dev/null || true
    find /src/.. -maxdepth 1 -name '*.tar.*' -exec cp {} /src/result/ \\; 2>/dev/null || true
    find /src/.. -maxdepth 1 -name '*.changes' -exec cp {} /src/result/ \\; 2>/dev/null || true
    find /src/.. -maxdepth 1 -name '*.buildinfo' -exec cp {} /src/result/ \\; 2>/dev/null || true
    ls -la /src/result/
"
podman cp "$CONTAINER:/src/result/." "$RESULT_DIR/"

echo "=== Source build complete ==="
ls -la "$RESULT_DIR/"
