#!/bin/bash
set -euo pipefail

# Publish .rpm packages to S3 YUM repository
#
# Required environment variables:
#   S3_BUCKET       - S3 bucket name
#   GPG_KEY_ID      - GPG key ID for signing
#   GPG_PASSPHRASE  - GPG passphrase
#
# Expected input:
#   rpms/*.rpm      - .rpm packages to publish (x86_64 and aarch64)

REPO_DIR=$(mktemp -d)
trap "rm -rf $REPO_DIR" EXIT

mkdir -p "$REPO_DIR/x86_64/Packages"
mkdir -p "$REPO_DIR/aarch64/Packages"

echo "=== Syncing existing packages from S3 ==="
aws s3 sync "s3://$S3_BUCKET/" "$REPO_DIR/" --quiet || true

echo "=== Copying new .rpm packages ==="
for rpm in rpms/*.rpm; do
    if [ -f "$rpm" ]; then
        basename_rpm=$(basename "$rpm")
        if [[ "$basename_rpm" == *".x86_64.rpm" ]]; then
            cp "$rpm" "$REPO_DIR/x86_64/Packages/"
        elif [[ "$basename_rpm" == *".aarch64.rpm" ]]; then
            cp "$rpm" "$REPO_DIR/aarch64/Packages/"
        else
            echo "Warning: Unknown architecture for $basename_rpm, skipping"
            continue
        fi
    fi
done

cd "$REPO_DIR"

echo "=== Generating repository metadata ==="
for arch in x86_64 aarch64; do
    cd "$REPO_DIR/$arch"
    createrepo_c --update .
    echo "$GPG_PASSPHRASE" | gpg --batch --yes --passphrase-fd 0 \
        --default-key "$GPG_KEY_ID" \
        --detach-sign --armor repodata/repomd.xml
done

cd "$REPO_DIR"

echo "=== Exporting public key ==="
gpg --armor --export "$GPG_KEY_ID" > "RPM-GPG-KEY-cachecannon"

echo "=== Uploading to S3 ==="
aws s3 sync "$REPO_DIR/" "s3://$S3_BUCKET/" \
    --delete \
    --exclude ".git/*" \
    --cache-control "max-age=300"

aws s3 cp "s3://$S3_BUCKET/RPM-GPG-KEY-cachecannon" "s3://$S3_BUCKET/RPM-GPG-KEY-cachecannon" \
    --content-type "application/pgp-keys" --metadata-directive REPLACE

echo "=== YUM repository published successfully ==="
