# Phantom Bridge

Minimal browser signing client for ClawSolana.

## What it does

1. Connects to Phantom wallet
2. Binds wallet to session via challenge-response ownership proof
3. Polls pending signature requests
4. Signs transactions with Phantom
5. Submits signed transactions back to the gateway

## How to use

```bash
# Serve the file (any static HTTP server works)
cd bridge
python3 -m http.server 8080
# Open http://localhost:8080
```

Fill in:
- **API Base**: `http://127.0.0.1:7070` (default)
- **Session ID**: UUID from `POST /sessions`
- **Bearer Token**: your `CLAW_API_TOKEN`

Then: **Connect** → **Bind** → **Refresh** → **Sign**

## Requirements

- Phantom browser extension installed
- ClawSolana daemon running (`clawd`)
- Active session with pending external wallet signature requests

## Dependencies

Uses ESM CDN imports (no build step):
- `@solana/web3.js` for transaction deserialization/serialization
- Phantom's injected `window.solana` provider

## Manual Test Flow

```bash
# 1. Start daemon
cd /path/to/ClawSolana
export $(grep -v '^#' .env | grep -v '^\s*$' | xargs)
cargo run --bin clawd -- --config config/default.toml

# 2. Create a session (in another terminal)
curl -s -X POST http://127.0.0.1:7070/sessions \
  -H "Authorization: Bearer $CLAW_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"role": "execution", "channel": "cli"}'
# Copy the session_id

# 3. Serve bridge
cd bridge && python3 -m http.server 8080
# Open http://localhost:8080 in browser with Phantom

# 4. In the bridge page:
#    - Paste session_id and token
#    - Click "Connect Wallet"
#    - Click "Bind Wallet" (Phantom will ask to sign the challenge message)
#    - Click "Refresh" to see pending requests
#    - Click "Sign" on any pending request
```

## Limitations

- No error recovery UI
- No auto-polling
- Hardcoded to single session
- No multi-wallet support
- Phantom only (no WalletConnect)
