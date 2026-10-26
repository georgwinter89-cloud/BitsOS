#!/bin/bash
# PK-4: Node.js (CachyOS's /usr/bin/node, 42 libraries: ICU, OpenSSL, libuv, ...) as a package on CachyOS:
# `bitspkg import` in both layouts (libpath: lib/ + LD_LIBRARY_PATH without the V2 glibc set; view: everything,
# glibc and ld.so too, started through the package's own ld.so), build, install into a scratch root, run
# `node -e` from the package, remove: the scratch root as before. In the view layout every library the loader
# names must come from the package. About 250 MB in target/, some seconds. (Too big for MCP's write_file in
# QEMU; jq covers V2 in 20-foreign-qemu.sh.)
set -euo pipefail
ROOT="${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
RUN="${BITSOS_RUN_DIR:-$ROOT/target/xtask/runs/pk-40-node-package}"
[ -x /usr/bin/node ] || { echo "SKIP: no /usr/bin/node"; exit 0; }
command -v ldd >/dev/null || { echo "SKIP: no ldd"; exit 0; }
rm -rf "$RUN"
mkdir -p "$RUN/src/bin"
(cd "$ROOT/user" && env -u CARGO_TARGET_DIR -u RUSTUP_TOOLCHAIN cargo build -q --offline --release -p bitspkg --target-dir "$ROOT/target/user-host")
B="$ROOT/target/user-host/release/bitspkg"
cp /usr/bin/node "$RUN/src/bin/node"
R="$RUN/root"
"$B" --root "$R" gc > /dev/null
"$B" snapshot "$R" | grep -v '^apps/.next-id ' > "$RUN/before.txt"
for n in libpath view; do
  "$B" import "$RUN/src" "$RUN/tree-$n" --system "$ROOT/user/packaging/libset.txt" --layout $n --launcher bin/node > "$RUN/import-$n.txt"
  printf 'format = 1\nname = "node-%s"\nversion = "%s"\nprogram = "bitsos/run"\n' $n "$(node --version | tr -d v)" > "$RUN/tree-$n/package.toml"
  "$B" build "$RUN/tree-$n" --out "$RUN/pkg-$n"
  "$B" --root "$R" install "$RUN/pkg-$n"
  rm -rf "$RUN/tree-$n" "$RUN/pkg-$n"
  P="$R/apps/node-$n/current"
  out=$(env -u LD_LIBRARY_PATH "$P/bitsos/run" -e 'console.log(6 * 7, process.versions.openssl)')
  echo "node-$n: $out ($(grep -c '^lib ' "$RUN/import-$n.txt") libraries in the package)"
  [ "${out%% *}" = 42 ] || { echo "RED: node from the $n package"; exit 1; }
  if [ $n = view ]; then
    foreign=$("$P/usr/lib/ld-linux-x86-64.so.2" --library-path "$P/usr/lib" --list "$P/bin/node" | grep '=>' | grep -v "$P/" || true)
    [ -z "$foreign" ] || { echo "RED: libraries from outside the package: $foreign"; exit 1; }
  fi
  "$B" --root "$R" remove "node-$n"
done
"$B" snapshot "$R" | grep -v '^apps/.next-id ' > "$RUN/after.txt"
diff "$RUN/before.txt" "$RUN/after.txt" || { echo "RED: leftovers"; exit 1; }
rm -rf "$RUN/src" "$R"
echo "Node as a package on CachyOS: green (both layouts, removed without leftovers)"
