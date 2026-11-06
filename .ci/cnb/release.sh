#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

if [[ "${GITHUB_REPOSITORY:-}" != JachinShen/prime-agent ||
      "${GITHUB_REF:-}" != refs/heads/personal-release ]]; then
    echo "Expected the Prime Agent personal-release source" >&2
    exit 1
fi
commit="$(git rev-parse HEAD)"
if [[ "$commit" != "${GITHUB_SOURCE_SHA:?GITHUB_SOURCE_SHA is required}" ]]; then
    echo "Checkout does not match the requested GitHub commit" >&2
    exit 1
fi

case "$(uname -sm)" in
    'Linux x86_64') target=x86_64-unknown-linux-gnu; platform=linux-x64 ;;
    'Linux aarch64') target=aarch64-unknown-linux-gnu; platform=linux-arm64 ;;
    *) echo "Unsupported native release host: $(uname -sm)" >&2; exit 1 ;;
esac

version="$(python3 - <<'PY'
import sys
sys.path.insert(0, "scripts")
from package_release import ROOT, workspace_version
print(workspace_version(ROOT))
PY
)"
mkdir -p "target/$target"
dist="$(mktemp -d "target/$target/cnb-dist.XXXXXX")"
trap 'rm -rf "$dist"' EXIT
catalog="target/$target/cnb-catalog"
publish="target/cnb-artifacts/$commit"
mkdir -p "$publish"

cargo build --release --locked --target "$target" -p pa-cli --bin prime-agent
python3 scripts/release/split_debug.py \
    --binary "target/$target/release/prime-agent" \
    --shipped "$dist/prime-agent" --out "$dist" \
    --version "$version" --target "$target"

# Refuse an artifact that no longer starts on the release's GLIBC baseline.
objdump -T "$dist/prime-agent" > "$dist/dynamic-symbols.txt"
python3 - "$dist/dynamic-symbols.txt" <<'PY'
from pathlib import Path
import re
import sys

versions = [tuple(map(int, value.split("."))) for value in
            re.findall(r"GLIBC_([0-9.]+)", Path(sys.argv[1]).read_text())]
if not versions:
    raise SystemExit("No GLIBC symbols found; cannot verify the release baseline")
highest = max(versions)
if highest > (2, 35):
    raise SystemExit(f"Required GLIBC {highest} exceeds the 2.35 release baseline")
print(f"GLIBC baseline verified: {highest}")
PY

# Both architectures use the same immutable catalog as the personal Nix package.
python3 scripts/release/bundle_catalog.py generate --network \
    --catalog-ref "${PRIME_AGENT_CATALOG_REF:?PRIME_AGENT_CATALOG_REF is required}" \
    --out "$catalog"
python3 scripts/release/assemble_artifacts.py \
    --repo-root . --version "$version" --target "$target" --sha "$commit" \
    --binary "$dist/prime-agent" \
    --decoder "$dist/prime-agent-$version-$platform.debug.gz" \
    --catalog-assets "$catalog" --out-dir "$dist"
python3 scripts/release/verify_release.py \
    --dist-dir "$dist" --version "$version" --target "$target" --sha "$commit"
python3 scripts/release/verify_decoders.py "$dist"

cp "$dist/prime-agent-$version-$platform.tar.gz" \
    "$dist/prime-agent-$version-$platform.debug.gz" "$publish/"
# Commit attachments share a namespace, so every metadata filename includes its platform.
cp "$dist/SHA256SUMS" "$publish/SHA256SUMS-$platform"
cp "$dist/manifest.json" "$publish/manifest-$platform.json"

python3 - "$publish/build-$platform.json" "$target" <<'PY'
import json
import os
from pathlib import Path
import subprocess
import sys

Path(sys.argv[1]).write_text(json.dumps({
    "commit": os.environ["GITHUB_SOURCE_SHA"],
    "controllerCommit": os.environ["CNB_COMMIT"],
    "target": sys.argv[2],
    "catalogCommit": os.environ["PRIME_AGENT_CATALOG_REF"],
    "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
    "buildUrl": os.environ["CNB_BUILD_WEB_URL"],
}, indent=2) + "\n")
PY

# CNB attaches to the controller commit. Include the source SHA in each bundle
# name so builds of different GitHub commits never replace one another.
bundle="prime-agent-$version-$commit-$platform-artifacts.tar.gz"
tar -czf "$publish/$bundle" -C "$publish" \
    "prime-agent-$version-$platform.tar.gz" \
    "prime-agent-$version-$platform.debug.gz" \
    "SHA256SUMS-$platform" "manifest-$platform.json" "build-$platform.json"
sha256sum "$publish/$bundle" | sed "s|$publish/||" > "$publish/$bundle.sha256"
