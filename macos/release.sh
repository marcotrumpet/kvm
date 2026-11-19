#!/bin/bash
# Publishes a KVM release that installed copies update to (Sparkle): builds with build.sh,
# signs the zip with the update key in the login keychain, writes appcast.xml and creates
# the GitHub release v<VERSION> with both. Bump VERSION in build.sh and commit first.
#   macos/release.sh ["notes, one item per line"]    (default: commit subjects since the last release)
set -euo pipefail

cd "$(dirname "$0")"
ROOT="$(cd .. && pwd)"
DIST="$ROOT/dist"
REPO="marcotrumpet/kvm"
VERSION="$(sed -n 's/^VERSION="\${VERSION:-\(.*\)}"$/\1/p' build.sh)"
TAG="v$VERSION"
SPARKLE_VERSION="2.10.1" # as in Package.swift
SPARKLE_SHA256="1a856d2a9edb8e628de88121d7be04cf0e2167f8641650b78fca00b14244cf27"
SPARKLE_DIR="$ROOT/target/sparkle"

[ -n "$VERSION" ] || { echo "error: no VERSION in build.sh"; exit 1; }
git -C "$ROOT" diff --quiet HEAD || { echo "error: commit your changes first (the release is built from HEAD)"; exit 1; }
if gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
    echo "error: $TAG is already released; bump VERSION in build.sh"
    exit 1
fi

if [ ! -x "$SPARKLE_DIR/bin/sign_update" ]; then
    echo "==> Sparkle tools"
    mkdir -p "$SPARKLE_DIR"
    curl -sL -o "$SPARKLE_DIR/Sparkle.tar.xz" \
        "https://github.com/sparkle-project/Sparkle/releases/download/$SPARKLE_VERSION/Sparkle-$SPARKLE_VERSION.tar.xz"
    echo "$SPARKLE_SHA256  $SPARKLE_DIR/Sparkle.tar.xz" | shasum -a 256 -c -
    tar -xJf "$SPARKLE_DIR/Sparkle.tar.xz" -C "$SPARKLE_DIR"
fi

NOTES="${1:-}"
if [ -z "$NOTES" ]; then
    PREVIOUS="$(gh release view --repo "$REPO" --json tagName -q .tagName 2>/dev/null || true)"
    if [ -n "$PREVIOUS" ] && git -C "$ROOT" rev-parse -q --verify "refs/tags/$PREVIOUS" >/dev/null; then
        NOTES="$(git -C "$ROOT" log --format='%s' "$PREVIOUS..HEAD")"
    else
        NOTES="$(git -C "$ROOT" log -1 --format='%s')"
    fi
fi

VERSION="$VERSION" ALLOW_UNNOTARIZED=1 ./build.sh

echo "==> Appcast"
# Prints: sparkle:edSignature="..." length="..."
SIGNATURE="$("$SPARKLE_DIR/bin/sign_update" "$DIST/KVM-macos.zip")"
VERSION="$VERSION" NOTES="$NOTES" SIGNATURE="$SIGNATURE" \
URL="https://github.com/$REPO/releases/download/$TAG/KVM-macos.zip" python3 - > "$DIST/appcast.xml" <<'EOF'
import email.utils, html, os
items = "".join(f"<li>{html.escape(l.lstrip('- '))}</li>" for l in os.environ["NOTES"].splitlines() if l.strip())
v = os.environ["VERSION"]
print(f"""<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
  <channel>
    <title>KVM</title>
    <item>
      <title>KVM {v}</title>
      <pubDate>{email.utils.formatdate()}</pubDate>
      <sparkle:version>{v}</sparkle:version>
      <sparkle:shortVersionString>{v}</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>13.0</sparkle:minimumSystemVersion>
      <description><![CDATA[<ul>{items}</ul>]]></description>
      <enclosure url="{os.environ['URL']}" type="application/octet-stream" {os.environ['SIGNATURE']}/>
    </item>
  </channel>
</rss>""")
EOF

echo "==> GitHub release $TAG"
gh release create "$TAG" "$DIST/KVM-macos.zip" "$DIST/appcast.xml" --repo "$REPO" \
    --target "$(git -C "$ROOT" rev-parse HEAD)" --title "KVM $VERSION" --notes "$NOTES"
echo "==> Released $TAG; installed copies find it within a day, or with Check for Updates…"
