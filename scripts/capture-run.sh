#!/usr/bin/env bash
# capture-run.sh — one-shot capture of a full local test run into
# docs/artifacts/<timestamp>/ so results are durable and diffable.
#
# Usage (from repo root):
#   bash scripts/capture-run.sh [--skip-playwright] [--skip-rust] [--force-build]
#
# What it does, in order:
#   1. Kill any stray clawd.exe (Windows LNK1104 mitigation).
#   2. Run `cargo nextest` under the `ci` profile if nextest is installed;
#      otherwise fall back to `cargo test` and tee stdout.
#   3. Copy the resulting junit.xml into the artifact dir.
#   4. Run the full Playwright suite if @playwright/test is installed.
#   5. Copy playwright-report/ into the artifact dir.
#
# It never runs the devnet E2E — that is deliberately opt-in (CLAW_DEVNET_KEYPAIR).

set -u
repo_root="$(cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$repo_root"

SKIP_PLAYWRIGHT=0
SKIP_RUST=0
FORCE_BUILD=0
for arg in "$@"; do
  case "$arg" in
    --skip-playwright) SKIP_PLAYWRIGHT=1 ;;
    --skip-rust)       SKIP_RUST=1 ;;
    --force-build)     FORCE_BUILD=1 ;;
    *) echo "unknown arg: $arg"; exit 2 ;;
  esac
done

ts="$(date -u +%Y-%m-%dT%H-%M-%SZ)"
out="$repo_root/docs/artifacts/$ts"
mkdir -p "$out"
echo "[capture-run] writing to $out"

# 1. Clear any existing daemon to avoid LNK1104 on Windows.
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) taskkill //F //IM clawd.exe >/dev/null 2>&1 || true ;;
  *) pkill -f target/debug/clawd >/dev/null 2>&1 || true ;;
esac

# 2 + 3. Rust tests.
if [ "$SKIP_RUST" -eq 0 ]; then
  echo "[capture-run] Rust tests"
  if command -v cargo-nextest >/dev/null 2>&1 || cargo nextest --version >/dev/null 2>&1; then
    cargo nextest run \
      --profile ci \
      -p claw-types -p claw-risk-engine -p claw-state-store \
      -p claw-api -p claw-gateway \
      2>&1 | tee "$out/rust-nextest.log" || true
    junit="$repo_root/target/nextest/ci/junit.xml"
    if [ -f "$junit" ]; then
      cp "$junit" "$out/rust-junit.xml"
      echo "[capture-run] copied junit.xml -> $out/rust-junit.xml"
    else
      echo "[capture-run] warning: nextest junit.xml not found at $junit"
    fi
  else
    echo "[capture-run] cargo-nextest not installed; falling back to cargo test"
    cargo test \
      -p claw-types -p claw-risk-engine -p claw-state-store \
      -p claw-api -p claw-gateway \
      2>&1 | tee "$out/rust-cargo-test.log" || true
  fi
fi

# 4 + 5. Playwright.
if [ "$SKIP_PLAYWRIGHT" -eq 0 ]; then
  if [ ! -f "$repo_root/frontend/node_modules/@playwright/test/package.json" ]; then
    echo "[capture-run] Playwright not installed; skipping UI tests"
  else
    echo "[capture-run] Playwright E2E"
    (
      cd "$repo_root/frontend"
      if [ "$FORCE_BUILD" -eq 1 ]; then
        export CLAW_E2E_FORCE_BUILD=1
      fi
      npx playwright test 2>&1 | tee "$out/playwright.log" || true
    )
    if [ -d "$repo_root/frontend/playwright-report" ]; then
      mkdir -p "$out/playwright-report"
      cp -R "$repo_root/frontend/playwright-report/." "$out/playwright-report/"
      echo "[capture-run] copied playwright-report/ -> $out/playwright-report/"
    fi
  fi
fi

# Summary marker so grep-based tooling can find the newest run.
{
  echo "# Capture run"
  echo ""
  echo "- Timestamp (UTC): $ts"
  echo "- Repo root: $repo_root"
  echo "- Rust skipped: $SKIP_RUST"
  echo "- Playwright skipped: $SKIP_PLAYWRIGHT"
  echo "- Force frontend rebuild: $FORCE_BUILD"
} > "$out/README.md"

echo "[capture-run] done — see $out"
