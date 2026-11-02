# FUN

The on-chain rules behind [fun](https://funpad.meme), a memecoin launchpad on Meteora.

This is a Token-2022 transfer hook. Every coin launched on fun points at it, so every transfer is checked against that coin's rules until the coin bonds.

| | |
|---|---|
| **Program** | [`FUNrSeDa9WjjJnAtZJpy7195jdee1VnWtEDU5nMW9Q7b`](https://solscan.io/account/FUNrSeDa9WjjJnAtZJpy7195jdee1VnWtEDU5nMW9Q7b) |
| **Network** | Solana mainnet |
| **Verified build** | [OtterSec status](https://verify.osec.io/status/FUNrSeDa9WjjJnAtZJpy7195jdee1VnWtEDU5nMW9Q7b) |
| **Contact** | [@launchviafun](https://x.com/launchviafun) |

## The rules

Each rule is fixed when a coin launches.

| Rule | What it does |
|---|---|
| **Max wallet** | No wallet can end a transfer holding more than the coin's cap, set between 0.5% and 4% of supply. |
| **Transfers** | Off by default: tokens only move through buys and sells. A launcher can turn wallet-to-wallet sends on. Receivers still have to respect the max wallet and blocklist. |
| **Quick bond** | Optional. Nobody can sell until the coin bonds or its sell lock runs out, at most 24 hours. Transfers stay off. |
| **Blocklist** | Optional. Up to 2,000 wallets can't receive the coin, so they can't buy it. Written before the pool exists and sealed at launch. Listed wallets can still sell or send what they already hold. |

The launcher's wallet is exempt from the max wallet and transfer rules. It handles buybacks and moves the dev buy into and out of its lock.

## When a coin bonds

Meteora's bonding curve revokes the hook in the swap that completes it. This program is never called for that coin again, so every rule above ends there.

## What isn't in here

Two other parts of a fun launch live outside this program:

- **Sniper fee:** the 99% fee on a coin's first blocks is set in its Meteora pool config.
- **Dev lock:** the 24-hour lock on a dev buy is held in [Jupiter Lock](https://lock.jup.ag), and carries on after bonding.

## Instructions

| Instruction | Purpose |
|---|---|
| `initialize` | Writes the coin's rules. Sent in the same transaction that creates the pool. |
| `open_blocklist` | Starts an empty blocklist before launch. |
| `add_blocked` | Adds wallets to an unsealed blocklist, in ascending order. |
| `close_blocklist` | Returns a blocklist's rent before launch, or after bonding. |
| `execute` | Called by Token-2022 on every transfer. Read-only. |

## Verify it yourself

The build is reproducible, so you can check the code here matches what's on-chain. You need [Docker](https://www.docker.com) and [solana-verify](https://github.com/Ellipsis-Labs/solana-verifiable-build).

```sh
solana-verify build --library-name unsniped_hook \
  -b solanafoundation/solana-verifiable-build:3.1.13

solana-verify get-executable-hash target/deploy/unsniped_hook.so
solana-verify get-program-hash FUNrSeDa9WjjJnAtZJpy7195jdee1VnWtEDU5nMW9Q7b
```

The two hashes should match.

The source is in [`programs/unsniped-hook/src/lib.rs`](programs/unsniped-hook/src/lib.rs). `unsniped` is the project's original name.
