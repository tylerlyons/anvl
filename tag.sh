#!/usr/bin/env bash
set -euo pipefail

VERSION=$(grep '^version' crates/tui/Cargo.toml | sed 's/version = "\(.*\)"/\1/')
TAG="v$VERSION"

if git rev-parse "$TAG" >/dev/null 2>&1; then
  echo "Tag $TAG already exists. Aborting."
  exit 1
fi

git tag -a "$TAG" -m "$TAG"
git push origin "$TAG"

echo "Created and pushed tag $TAG"
