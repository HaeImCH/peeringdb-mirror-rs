#!/usr/bin/env bash
#
# Build script for Cloudflare Workers Builds (native GitHub integration).
#
# In the Cloudflare dashboard (Worker > Settings > Builds) set:
#   Build command:  bash scripts/ci-build.sh
#   Deploy command: npx wrangler deploy   (the default)
#
# Workers Builds runs the build command and then the deploy command in the
# same checkout, so the wrangler.toml generated here is what `wrangler deploy`
# uploads. The Rust toolchain is NOT preinstalled in the build image, so we
# install it below.
set -euo pipefail

# --- Generate wrangler.toml -------------------------------------------------
# D1_DATABASE_ID is set as a Build variable in the dashboard
# (Worker > Settings > Builds > Variables and secrets). It is just an
# identifier, not a credential. We intentionally omit a [build] section so the
# deploy step's `wrangler deploy` uploads the artifact built below instead of
# trying to rebuild without cargo on PATH.
: "${D1_DATABASE_ID:?D1_DATABASE_ID build variable is not set in Workers Builds}"

cat > wrangler.toml <<EOF
name = "peeringdb-mirror"
main = "build/worker/shim.mjs"
compatibility_date = "2024-04-01"
compatibility_flags = ["nodejs_compat"]

[triggers]
crons = ["0 */3 * * *"] # run sync every 3 hours

[[d1_databases]]
binding = "PEERINGDB"
database_name = "peeringdb-mirror"
database_id = "${D1_DATABASE_ID}"
EOF
echo "Generated wrangler.toml"

# --- Install the Rust toolchain (absent from the build image) ---------------
if ! command -v cargo >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
# shellcheck disable=SC1091
. "$HOME/.cargo/env"
rustup target add wasm32-unknown-unknown

# --- Build the Worker -------------------------------------------------------
cargo install -q worker-build --version 0.7.2 --locked
worker-build --release
