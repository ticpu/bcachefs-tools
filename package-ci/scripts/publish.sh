#!/bin/bash
# Publish built .deb packages to apt.bcachefs.org
#
# Signs .debs with debsigs, then includes them in aptly repos and publishes.
# Called by the orchestrator after all builds for a commit succeed.
#
# Usage: publish.sh COMMIT [snapshot|release]
#   SUITE defaults to "snapshot" (use "release" for tagged releases)
#
# Config read from $STATE_DIR/config:
#   GPG_SIGNING_SUBKEY_FINGERPRINT
#   APTLY_ROOT

set -euo pipefail

COMMIT="$1"
SUITE="${2:-snapshot}"

STATE_DIR="${STATE_DIR:-/home/aptbcachefsorg/package-ci}"
BUILD_DIR="$STATE_DIR/builds/$COMMIT"
SHORT="${COMMIT:0:12}"

# Load config
# shellcheck source=/dev/null
source "$STATE_DIR/config"
: "${GPG_SIGNING_SUBKEY_FINGERPRINT:?not set in config}"
: "${APTLY_ROOT:?not set in config}"

SNAPSHOT_DATE="$(date -u +%Y%m%d%H%M%S)"

# Minimal aptly config — points at the existing db/pool/public
APTLY_CONF="$(mktemp)"
trap "rm -f '$APTLY_CONF'" EXIT
cat > "$APTLY_CONF" << EOF
{
    "rootDir": "$APTLY_ROOT",
    "gpgDisableVerify": true,
    "skipContentsPublishing": true,
    "FileSystemPublishEndpoints": {
        "public": {
            "rootDir": "$APTLY_ROOT/public",
            "linkMethod": "symlink"
        }
    }
}
EOF

aptly() { command aptly -config="$APTLY_CONF" "$@"; }

echo "=== Publishing $SHORT (suite=$SUITE) ==="

SRC_DIR="$BUILD_DIR/source/result"
if [ ! -d "$SRC_DIR" ]; then
    echo "ERROR: no source result dir at $SRC_DIR"
    exit 1
fi

sign_debs() {
    local dir="$1"
    find "$dir" -maxdepth 1 \( -name "*.deb" -o -name "*.ddeb" \) | while read -r deb; do
        echo "  signing $(basename "$deb")"
        debsigs --verbose --default-key="$GPG_SIGNING_SUBKEY_FINGERPRINT" --sign=origin "$deb"
    done
}

echo "--- Signing source artifacts ---"
sign_debs "$SRC_DIR"

# Collect which distros have at least one successful arch build
declare -A DISTRO_DONE
for job_dir in "$BUILD_DIR"/*/; do
    job="$(basename "$job_dir")"
    [ "$job" = "source" ] && continue
    status="$(cat "$job_dir/status" 2>/dev/null || echo pending)"
    [ "$status" != "done" ] && continue
    distro="${job%-*}"
    DISTRO_DONE["$distro"]=1
    echo "--- Signing $job ---"
    sign_debs "$job_dir/result"
done

# Include, snapshot, publish per distro
for distro in "${!DISTRO_DONE[@]}"; do
    REPO_NAME="$distro-$SUITE"
    REPO_SUITE="bcachefs-tools-$SUITE"
    SNAPSHOT_NAME="$REPO_NAME-$SNAPSHOT_DATE"
    PUBLISH_PREFIX="filesystem:public:$distro"

    echo "--- $distro: including into $REPO_NAME ---"

    aptly repo show "$REPO_NAME" &>/dev/null || \
        aptly repo create \
            -distribution="$REPO_SUITE" \
            -component=main \
            "$REPO_NAME"

    # Build list of dirs to include: source + all arches for this distro
    INCLUDE_DIRS=("$SRC_DIR")
    for job_dir in "$BUILD_DIR/${distro}"-*/; do
        [ -d "$job_dir/result" ] && \
            [ "$(cat "$job_dir/status" 2>/dev/null)" = "done" ] && \
            INCLUDE_DIRS+=("$job_dir/result")
    done

    # repo add takes .deb/.dsc files directly (avoids needing signed .changes)
    aptly repo add -force-replace "$REPO_NAME" "${INCLUDE_DIRS[@]}"

    echo "--- $distro: snapshot $SNAPSHOT_NAME ---"
    aptly snapshot create "$SNAPSHOT_NAME" from repo "$REPO_NAME"

    echo "--- $distro: publish ---"
    if aptly publish show "$REPO_SUITE" "$PUBLISH_PREFIX" &>/dev/null; then
        aptly publish switch -force-overwrite \
            "$REPO_SUITE" "$PUBLISH_PREFIX" "$SNAPSHOT_NAME"
    else
        aptly publish snapshot \
            -force-overwrite \
            -acquire-by-hash \
            -origin="apt.bcachefs.org" \
            -label="apt.bcachefs.org Packages" \
            "$SNAPSHOT_NAME" \
            "$PUBLISH_PREFIX"
    fi
done

echo "=== Publish complete: $SHORT ==="
