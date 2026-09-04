#!/usr/bin/env bash
# Build the npm package this fork publishes.
#
# The fork carries a change upstream does not have (directives report the
# file and lines they were written on), so it ships under its own scope as
# `@note-kdia/rustledger-wasm` rather than shadowing `@rustledger/wasm`.
#
# The crate version stays on upstream's so rebases stay quiet; the npm
# version is stamped here instead. Name it after the upstream base plus this
# fork's patch series, e.g. upstream 0.22.0 -> `0.22.0-location.1`.
#
# Usage: scripts/build-fork-npm.sh <npm version>
# Then:  cd crates/rustledger-wasm/pkg && npm publish --access public
set -euo pipefail

VERSION="${1:-}"
if [ -z "$VERSION" ]; then
    echo "usage: scripts/build-fork-npm.sh <npm version, e.g. 0.22.0-location.1>" >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WASM_DIR="$(cd "$SCRIPT_DIR/../crates/rustledger-wasm" && pwd)"

# `--target web`: the same build the upstream npm package ships, and the one
# boki loads (wasm-bindgen's web target needs no bundler plugin).
wasm-pack build --target web --release --scope note-kdia "$WASM_DIR"

# wasm-pack takes the version from Cargo.toml, which tracks upstream. Stamp
# the fork's own version and point the metadata at the fork, so an installed
# copy says where it came from.
python3 - "$WASM_DIR/pkg/package.json" "$VERSION" <<'PY'
import json
import sys

path, version = sys.argv[1], sys.argv[2]
with open(path) as f:
    package = json.load(f)

package["version"] = version
package["repository"] = {
    "type": "git",
    "url": "https://github.com/note-kdia/rustledger",
}
package["description"] = (
    "Beancount WebAssembly bindings for JavaScript/TypeScript "
    "(note-kdia fork: directives carry their source location)"
)

with open(path, "w") as f:
    json.dump(package, f, indent=2)
    f.write("\n")
PY

echo "built $WASM_DIR/pkg ($VERSION)" >&2
