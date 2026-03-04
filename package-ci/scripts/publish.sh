#!/bin/bash
# Publish built .deb packages to the apt.bcachefs.org repository
#
# Uses aptly to manage the Debian repository locally (no sshfs).
#
# Usage: publish.sh COMMIT BUILD_DIR APTLY_ROOT

set -euo pipefail

COMMIT="$1"
BUILD_DIR="$2"
APTLY_ROOT="$3"

SNAPSHOT_DATE=$(date -u +%Y%m%d%H%M%S)

echo "=== Publishing packages for ${COMMIT:0:12} ==="

# Write aptly config (idempotent)
APTLY_CONF="$APTLY_ROOT/aptly.conf"
cat > "$APTLY_CONF" << EOF
{
    "rootDir": "$APTLY_ROOT",
    "gpgDisableVerify": true,
    "FileSystemPublishEndpoints": {
        "public": {
            "rootDir": "$APTLY_ROOT/public",
            "linkMethod": "symlink"
        }
    }
}
EOF

aptly="aptly -config=$APTLY_CONF"

# Collect all .deb and .dsc files from build results
INCOMING=$(mktemp -d)
trap 'rm -rf "$INCOMING"' EXIT

# Copy source files
if [ -d "$BUILD_DIR/source/result" ]; then
    cp "$BUILD_DIR/source/result"/*.dsc "$INCOMING/" 2>/dev/null || true
    cp "$BUILD_DIR/source/result"/*.tar.* "$INCOMING/" 2>/dev/null || true
    cp "$BUILD_DIR/source/result"/*.changes "$INCOMING/" 2>/dev/null || true
fi

# Determine which distros have successful builds
DISTROS=()
for dir in "$BUILD_DIR"/*/; do
    dirname=$(basename "$dir")
    # Skip source and publish dirs
    case "$dirname" in
        source|publish) continue ;;
    esac
    # Check if the job succeeded
    if [ -f "$dir/status" ] && [ "$(cat "$dir/status")" = "done" ]; then
        # Extract distro name from job name (e.g., "trixie-amd64" -> "trixie")
        distro="${dirname%-*}"
        if [[ ! " ${DISTROS[*]:-} " =~ " $distro " ]]; then
            DISTROS+=("$distro")
        fi
    fi
done

echo "Publishing for distros: ${DISTROS[*]}"

for distro in "${DISTROS[@]}"; do
    SUITE="bcachefs-tools-${distro}"
    REPO_NAME="${distro}"

    # Create repo if it doesn't exist
    if ! $aptly repo show "$REPO_NAME" &>/dev/null; then
        echo "Creating repo: $REPO_NAME"
        $aptly repo create \
            -distribution="$SUITE" \
            -component=main \
            "$REPO_NAME"
    fi

    # Collect .deb files for this distro (all arches)
    DISTRO_INCOMING=$(mktemp -d)
    for dir in "$BUILD_DIR"/${distro}-*/; do
        if [ -d "$dir/result" ]; then
            cp "$dir/result"/*.deb "$DISTRO_INCOMING/" 2>/dev/null || true
            cp "$dir/result"/*.ddeb "$DISTRO_INCOMING/" 2>/dev/null || true
        fi
    done
    # Also add source
    cp "$INCOMING"/*.dsc "$DISTRO_INCOMING/" 2>/dev/null || true
    cp "$INCOMING"/*.tar.* "$DISTRO_INCOMING/" 2>/dev/null || true

    if [ -z "$(ls -A "$DISTRO_INCOMING" 2>/dev/null)" ]; then
        echo "No packages found for $distro, skipping"
        rm -rf "$DISTRO_INCOMING"
        continue
    fi

    echo "Adding packages to repo: $REPO_NAME"
    $aptly repo add -force-replace "$REPO_NAME" "$DISTRO_INCOMING/"

    # Create snapshot
    SNAPSHOT="${REPO_NAME}-${SNAPSHOT_DATE}"
    echo "Creating snapshot: $SNAPSHOT"
    $aptly snapshot create "$SNAPSHOT" from repo "$REPO_NAME"

    # Publish or switch
    if $aptly publish show "$SUITE" filesystem:public: &>/dev/null; then
        echo "Switching publish: $SUITE"
        $aptly publish switch \
            -acquire-by-hash \
            "$SUITE" \
            "filesystem:public:" \
            "$SNAPSHOT"
    else
        echo "Initial publish: $SUITE"
        $aptly publish snapshot \
            -acquire-by-hash \
            -origin="apt.bcachefs.org" \
            -label="apt.bcachefs.org Packages" \
            "$SNAPSHOT" \
            "filesystem:public:"
    fi

    rm -rf "$DISTRO_INCOMING"
done

echo "=== Publish complete ==="
echo "Published distros: ${DISTROS[*]}"
