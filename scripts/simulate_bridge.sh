#!/usr/bin/env bash
#
# simulate_bridge.sh — Manual external signer bridge simulation
#
# Prerequisites:
#   - Running clawd instance
#   - Active session with an external wallet bound
#   - A pending wallet signature request
#
# Usage:
#   ./scripts/simulate_bridge.sh <session_id> [api_token] [base_url]
#
# This script:
#   1. Lists pending wallet signature requests
#   2. Extracts the first unsigned_tx_b64 and request_id
#   3. Prints them for manual signing
#
# To complete the flow, sign the transaction externally and submit via:
#   curl -X POST "$BASE/sessions/$SESSION_ID/wallet-signatures" \
#     -H "Authorization: Bearer $TOKEN" \
#     -H "Content-Type: application/json" \
#     -d '{"request_id": "...", "signed_tx_b64": "..."}'

set -euo pipefail

SESSION_ID="${1:?Usage: $0 <session_id> [api_token] [base_url]}"
TOKEN="${2:-${CLAW_API_TOKEN:-test-token}}"
BASE="${3:-http://127.0.0.1:7070}"

echo "=== External Signer Bridge Simulation ==="
echo "Session:  $SESSION_ID"
echo "Base URL: $BASE"
echo ""

# Step 1: List pending
echo "--- Step 1: Listing pending wallet signature requests ---"
PENDING=$(curl -s "$BASE/sessions/$SESSION_ID/wallet-signatures" \
  -H "Authorization: Bearer $TOKEN")

echo "$PENDING" | python3 -m json.tool 2>/dev/null || echo "$PENDING"
echo ""

COUNT=$(echo "$PENDING" | python3 -c "import sys,json; print(json.load(sys.stdin).get('pending_count', 0))" 2>/dev/null || echo "?")

if [ "$COUNT" = "0" ]; then
    echo "No pending requests. The agent must call submit_for_signing first."
    echo ""
    echo "To bind an external wallet:"
    echo "  curl -X POST '$BASE/sessions/$SESSION_ID/bind-wallet' \\"
    echo "    -H 'Authorization: Bearer $TOKEN' \\"
    echo "    -H 'Content-Type: application/json' \\"
    echo "    -d '{\"pubkey\": \"YOUR_WALLET_PUBKEY\"}'"
    exit 0
fi

# Step 2: Extract first request
REQUEST_ID=$(echo "$PENDING" | python3 -c "import sys,json; print(json.load(sys.stdin)['requests'][0]['request_id'])" 2>/dev/null)
UNSIGNED_TX=$(echo "$PENDING" | python3 -c "import sys,json; print(json.load(sys.stdin)['requests'][0]['unsigned_tx_b64'])" 2>/dev/null)
SIGNER=$(echo "$PENDING" | python3 -c "import sys,json; print(json.load(sys.stdin)['requests'][0]['expected_signer'])" 2>/dev/null)

echo "--- Step 2: Pending request details ---"
echo "Request ID:      $REQUEST_ID"
echo "Expected signer: $SIGNER"
echo "Unsigned TX B64:  ${UNSIGNED_TX:0:80}..."
echo ""

echo "--- Step 3: Sign and submit ---"
echo "To submit a signed transaction:"
echo ""
echo "  curl -X POST '$BASE/sessions/$SESSION_ID/wallet-signatures' \\"
echo "    -H 'Authorization: Bearer $TOKEN' \\"
echo "    -H 'Content-Type: application/json' \\"
echo "    -d '{\"request_id\": \"$REQUEST_ID\", \"signed_tx_b64\": \"<SIGNED_TX_B64>\"}'"
echo ""
echo "The signed_tx_b64 must be the same transaction (identical message bytes)"
echo "signed by the wallet at pubkey: $SIGNER"
