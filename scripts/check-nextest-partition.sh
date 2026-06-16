#!/usr/bin/env sh
# Verify the `unit` and `proptest` nextest profiles exactly partition the suite:
# every test runs in exactly one of the two parallel CI lanes, so none is dropped
# and none runs twice. Fails if the lanes do not sum to the whole suite, or if the
# property-test lane is empty (a sign the `proptest`-module naming convention that
# drives the selection has been broken).
#
# Reuses already-built test binaries when present (it does not force a rebuild
# beyond what `cargo nextest list` needs).
set -eu

count() {
  cargo nextest list --workspace --locked \
    --profile "$1" --message-format json 2>/dev/null \
  | python3 -c 'import sys, json
d = json.load(sys.stdin)
print(sum(
    1
    for s in d["rust-suites"].values()
    for t in s["testcases"].values()
    if (t.get("filter-match") or {}).get("status") == "matches"
))'
}

total=$(count ci)
unit=$(count unit)
prop=$(count proptest)
sum=$((unit + prop))

echo "total=$total  unit=$unit  proptest=$prop  (unit+proptest=$sum)"

rc=0
if [ "$sum" -ne "$total" ]; then
  echo "ERROR: unit + proptest ($sum) != total ($total): the lanes are not a partition." >&2
  rc=1
fi
if [ "$prop" -eq 0 ]; then
  echo "ERROR: the proptest lane is empty; the property-test selection is broken." >&2
  rc=1
fi
if [ "$rc" -eq 0 ]; then
  echo "Partition OK."
fi
exit "$rc"
