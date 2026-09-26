#!/usr/bin/env bash
# Enforces the kernel boundary from the design doc: the kernel's dependency
# tree must not contain async runtimes / HTTP clients, and its sources must not
# touch the filesystem, clocks, hash-ordered containers or accept closures.
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0
tree=$(cargo tree -p agent-kernel -e normal --prefix none --format '{p}' | awk '{print $1}' | sort -u)
for banned in tokio reqwest hyper async-std mio; do
  if grep -qx "$banned" <<<"$tree"; then
    echo "kernel depends on banned crate: $banned"; fail=1
  fi
done

src=crates/kernel/src
check() { # pattern, message
  if grep -rnE "$1" "$src" --include='*.rs' | grep -vE '^\S+:[0-9]+:\s*//' ; then
    echo "kernel purity violation: $2"; fail=1
  fi
}
check 'std::fs|std::net|std::process|std::env' "IO modules"
check 'SystemTime|Instant::now' "clock reads"
check '\bHashMap\b|\bHashSet\b' "hash-ordered containers (use BTreeMap/BTreeSet)"
check 'pub fn [a-z_]+(<[^>]*>)?\([^)]*(impl Fn|dyn Fn|FnMut|FnOnce)' "public API accepting closures"

if [ "$fail" -ne 0 ]; then exit 1; fi
echo "kernel purity: ok"
