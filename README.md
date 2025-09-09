# Auto-Claim GUI (Rust)

A native desktop app to auto-claim airdrops and auto-forward funds/tokens.
- Auto Claim tab: watch for ETH deposits and call `claim()` on your airdrop contract, with optional auto-forward of ETH or ERC‑20 tokens.
- Auto transfer tab: monitor any ERC‑20 token in your wallet and auto-forward to a destination.
- Settings: RPC, fallbacks (Alchemy/Infura), auto-claim thresholds, wallet import.

## Prerequisites
- Windows 10/11 (PowerShell) or any OS supported by Rust
- Rust toolchain (stable): https://rustup.rs
- Git (optional)

## Start from zero (new users)
### 1) Install Rust
- Offical website : https://www.rust-lang.org/tools/install
- Windows (PowerShell):
```powershell
winget install --id Rustlang.Rustup -e
rustup default stable
```
If asked for MSVC build tools, install “Desktop development with C++” via Visual Studio Build Tools: https://visualstudio.microsoft.com/visual-cpp-build-tools/

- macOS (Terminal):
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
```

- Ubuntu/Debian (Terminal):
```bash
sudo apt update && sudo apt install -y build-essential curl pkg-config libssl-dev
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
```

Verify:
```bash
rustc -V
cargo -V
```

### 2) Install Git (if needed)
- Windows: `winget install Git.Git`
- macOS: `brew install git`
- Ubuntu/Debian: `sudo apt install -y git`

### 3) Get the project
Option A (Git):
```bash
git clone https://github.com/MrFadiAi/Auto-claimer.git
cd Auto-claimer
```
Option B (ZIP): download ZIP, extract, open `Auto-claimer` in a terminal.

### 4) Run the app
```bash
cargo run --release
```
Notes:
- First run downloads dependencies (may take a few minutes).
- Binary: `target/release/linea-autoclaim[.exe]`

The app window will open as “Auto-Claim”.

## First-time setup
1) Import wallet (Settings → Wallet Settings):
   - Paste your private key (0x…), click “Import Wallet”.
   - Address will be shown. Keystore is saved at `%USERPROFILE%\.linea-autoclaim\keystore.json` (plaintext `pk_hex`).

2) Connection settings (Settings → Connection Settings):
   - RPC Endpoint: your main RPC (e.g. `https://rpc.linea.build` or Base/others).
   - Fallback RPCs: one per line (e.g. Alchemy/Infura URLs). The app will try primary, then fallbacks.
   - Auto-claim Thresholds:
     - Min deposit (wei): minimum ETH increase to trigger claim (default small number like 1).
     - Check interval (s): polling interval for auto-claim.
   - Save Connection Settings.

3) Auto-forward settings (Auto Claim tab):
   - Airdrop Contract Address: contract exposing no-arg `claim()`.
   - Claimed token address (optional): ERC‑20 token to forward after claim.
   - Destination address: where ETH/tokens will be sent.
   - Gas reserve (wei): keep this amount to cover gas; remainder forwarded.
   - Click “Save Auto-forward Settings”.

## Using the app
### Auto Claim tab
- Start Auto-claim: enters watch mode. When a deposit ≥ Min deposit is detected, it calls `claim()` and (if enabled) auto‑forwards.
- Stop Auto-claim: stops the watcher.
- Claim Now: sends `claim()` immediately and (if enabled) auto‑forwards.
- Logs panel (right): shows RPC selection, claim progress, forwarding results.

### Auto transfer tab
- Enter an ERC‑20 token address to monitor.
- Interval (s): how often to check your token balance (default 1s or your setting).
- Start: polls balance; if > 0, forwards full balance to destination with detailed logs.
- Stop: cancels watcher.

## RPC fallback behavior
- On every operation, the app tries the primary RPC, then each fallback.
- Health check: 3s timeout on `chainId`. Logs which endpoint is used.

## Network & balance indicators
- Wallet Status shows current network (by chainId) and balance on the selected RPC.
- Refreshes automatically ~every 20s and on RPC change.

## Security notes
- Private key is stored unencrypted per your project design at `%USERPROFILE%\.linea-autoclaim\keystore.json`.
- Delete this file to remove the wallet.

## Troubleshooting
- Button text color compile error: we use RichText; ensure `eframe = "0.27"`.
- Rate limits: reduce intervals or add private RPCs. For real-time, use WebSocket RPCs (future enhancement).
- Claim reverts: verify airdrop contract, allocation, and that address hasn’t claimed yet.

## Credits & Donate
- Built by MrCrypto — Twitter: https://x.com/Mr_CryptoYT
- Donate:
  - ETH: `0x519e9aa581E8A00cf4aa51ffc85B5E2BD2BECA75`
  - SOL: `5FW6WHGZFReH7XYHezhZijxPNtDVZjVLr3xffHrTFtzS`
  - BTC: `33vsHnSafGMV6atqAqppDEBiFenCipQ4do`


