#!/usr/bin/env bash
#
# Golden-response check for API behaviour. Records status + body for a fixed
# set of requests against a running local worker, so a refactor can prove it
# changed nothing.
#
#   wrangler dev --local --port 8787 &      # in another shell
#   scripts/golden.sh capture               # on the baseline code
#   scripts/golden.sh diff                  # on the changed code
#
# BASE overrides the target (default http://127.0.0.1:8787). Output lives in
# tasks/golden/ (gitignored): baseline/ from capture, current/ from diff.
set -euo pipefail

cmd="${1:-}"
BASE="${BASE:-http://127.0.0.1:8787}"
root="$(cd "$(dirname "$0")/.." && pwd)/tasks/golden"

REQUESTS=(
  "/api/net?asn=13335"
  "/api/net?asn=99999999"
  "/api/netixlan?asn=13335"
  "/api/net?info_never_via_route_servers=1"
  "/api/net?info_never_via_route_servers=true&limit=20"
  "/api/net?asn__in=13335,15169"
  "/api/net?name=cloudflare"
  "/api/net?name__contains=hurricane"
  "/api/net?name__startswith=google"
  "/api/net?asn__gte=4200000000&limit=10"
  "/api/netixlan?asn=13335&limit=5&skip=2"
  "/api/netixlan?net_id=694&limit=10"
  "/api/net?since=1700000000&limit=50"
  "/api/net?limit=3"
  "/api/net/1"
  "/api/net/abc"
  "/api/net?limit=x"
  "/api/net?id=999999999"
  "/api/bogus"
)

fetch_all() {
  local tmp; tmp="$(mktemp)"
  local out="$1"
  rm -rf "$out"
  mkdir -p "$out"
  local i=0
  for path in "${REQUESTS[@]}"; do
    i=$((i + 1))
    local f
    f="$(printf '%s/%02d.txt' "$out" "$i")"
    {
      echo "GET $path"
      # Same request twice: the second may come from the response cache and
      # must be identical to the first.
      for _ in 1 2; do
        curl -sS -o "$tmp" -w '%{http_code}\n' "$BASE$path"
        cat "$tmp"
        echo
      done
    } >"$f"
  done
  rm -f "$tmp"
  echo "fetched ${#REQUESTS[@]} requests into $out"
}

case "$cmd" in
  capture) fetch_all "$root/baseline" ;;
  diff)
    [ -d "$root/baseline" ] || { echo "no baseline; run capture first" >&2; exit 2; }
    fetch_all "$root/current"
    if diff -r "$root/baseline" "$root/current" >"$root/diff.txt"; then
      echo "golden: no differences"
    else
      echo "golden: DIFFERENCES (see $root/diff.txt)" >&2
      head -40 "$root/diff.txt" >&2
      exit 1
    fi
    ;;
  *) echo "usage: $0 capture|diff" >&2; exit 2 ;;
esac
