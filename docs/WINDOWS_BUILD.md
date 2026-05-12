# Windows Build Notes

`solana-sdk` (via `solana-secp256r1-program` → `openssl-sys`) needs a system
OpenSSL on Windows. The repo-wide `.cargo/config.toml` intentionally does
**not** hardcode the OpenSSL paths — pinning Windows-only absolute paths in
a workspace Cargo config would break Linux and macOS builds.

If `cargo check` / `cargo build` / `cargo test` fails on Windows with an
`openssl-sys` linker error, install OpenSSL separately and export the
paths in your shell **before** building.

## Recommended setup

1. Install the prebuilt OpenSSL for Windows 64-bit (e.g. from
   [`slproweb.com/products/Win32OpenSSL.html`](https://slproweb.com/products/Win32OpenSSL.html))
   into the default location:

   ```
   C:\Program Files\OpenSSL-Win64
   ```

2. Export the build env vars in your current shell session (PowerShell):

   ```powershell
   $env:OPENSSL_DIR         = "C:\Program Files\OpenSSL-Win64"
   $env:OPENSSL_LIB_DIR     = "C:\Program Files\OpenSSL-Win64\lib\VC\x64\MD"
   $env:OPENSSL_INCLUDE_DIR = "C:\Program Files\OpenSSL-Win64\include"
   $env:OPENSSL_NO_VENDOR   = "1"
   ```

   Or in cmd.exe:

   ```cmd
   set OPENSSL_DIR=C:\Program Files\OpenSSL-Win64
   set OPENSSL_LIB_DIR=C:\Program Files\OpenSSL-Win64\lib\VC\x64\MD
   set OPENSSL_INCLUDE_DIR=C:\Program Files\OpenSSL-Win64\include
   set OPENSSL_NO_VENDOR=1
   ```

3. Build / test as normal:

   ```
   cargo check
   cargo test
   ```

If your OpenSSL is installed elsewhere, adjust the three `OPENSSL_*_DIR`
paths accordingly. The `OPENSSL_NO_VENDOR=1` flag tells `openssl-sys` not
to fall back to the vendored OpenSSL crate.

## macOS / Linux

No special setup is required. `openssl-sys` resolves a system OpenSSL via
`pkg-config` (`libssl-dev` / `openssl-devel` / `brew install openssl`).
The repo-wide Cargo config does not set any platform-specific env vars,
so a `cargo check` on a fresh macOS or Linux clone should succeed.
