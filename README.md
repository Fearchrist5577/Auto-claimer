Linea Autoclaim (Rust)

Fast CLI to import an EVM wallet, watch for new ETH on Linea, and auto-call `claim()` on the airdrop contract.

Contract: 0x87bAa1694381aE3eCaE2660d97fe60404080Eb64

Build

```powershell
cd D:\SynologyDrive\Ai\Linea-autoclaim\linea-autoclaim
cargo build --release
```

Binary: `target\release\linea-autoclaim.exe`

Commands

1) Import wallet (private key hex)

```powershell
target\release\linea-autoclaim.exe import --pk-hex 0xyourprivatekeyhex
```

2) Show address (fund this with ETH on Linea)

```powershell
target\release\linea-autoclaim.exe address
```

3) Watch balance and auto-claim

```powershell
target\release\linea-autoclaim.exe --rpc https://rpc.linea.build watch-and-claim --min-delta-wei 1 --interval 8
```

4) Manual claim now

```powershell
target\release\linea-autoclaim.exe --rpc https://rpc.linea.build claim
```

Keystore

- Stored at `%USERPROFILE%\.linea-autoclaim\keystore.json`
- Plaintext JSON containing `pk_hex` (no password). Delete to remove the wallet.

Notes

- Default RPC is `https://rpc.linea.build`. Override with `--rpc`.
- Assumes contract exposes a no-arg `claim()`.
- Delete the keystore file to remove the wallet.


