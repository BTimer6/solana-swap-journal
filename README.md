# Solana Swap Journal

A local Rust CLI for importing swap fills, logging a pre-trade ExactIn quote, tracking one signed submission at a time, and exporting CSV from SQLite.

## Build

```sh
cargo build --locked
cargo run --locked -- --help
```

Set `RPC_URL` for history and confirmation checks. It defaults to the public mainnet RPC. `SEND_RPC_URL` optionally selects a separate standard `sendTransaction` endpoint on the same cluster. Use `--db` or `JOURNAL_DB` to select a SQLite file. Keep separate database files per cluster. The default file is `journal.sqlite3`.

## Import and review

The following read-only example uses a publicly observed mainnet swap from the [sample trade export](examples/public-trades.csv) and [sample daily summary](examples/public-daily.csv). The wallet and signature are sample data.

```sh
cargo run --locked -- --db sample.sqlite3 import --wallet A8BNMXfxCkjuEJ83piEBKdDydk1RVeDQ7jYoUFCTSWuv --signature Gi44zBwsd8eUGEVPS1jstts457hKLbm8SSMLrRVHVK2McrhJjosiszb65U1LdrjsF1WfCXoesLMhm8RX3dchx4s
cargo run --locked -- --db sample.sqlite3 report --trades --output trades.csv
cargo run --locked -- --db sample.sqlite3 report --output daily.csv
```

For wallet history, set `WALLET` to the public wallet address and `SWAP_PROGRAM` to the on-chain program address of a route or swap program you use. Supply an archival RPC when backfilling older signatures.

```sh
cargo run --locked -- import --wallet "$WALLET" --program "$SWAP_PROGRAM" --since 2026-11-01
cargo run --locked -- import --wallet "$WALLET" --program "$SWAP_PROGRAM"
cargo run --locked -- report --wallet "$WALLET" --since 2026-11-01 --until 2026-11-07 --output week.csv
```

Repeat `--program` for multiple programs. The cursor is scoped to the wallet and program set, and advances after a complete scan. Use `--since` to backfill an earlier date and `--since` with `--until` for a bounded UTC window. Date endpoints are inclusive. CSV output paths must be new files.

## Record a new submission

1. Save the quote you see before signing. Supply mint addresses and amounts in their smallest units as shown by the quote. Use the wrapped SOL mint (`So11111111111111111111111111111111111111112`) for SOL.
2. Use your own transaction builder and signer to produce one base64-encoded, fully signed legacy or v0 transaction. The CLI takes the signed wire transaction only. The wallet passed to `watch` must be one of its signers.
3. Submit explicitly. The tool journals the signature before the send, uses normal RPC preflight, and checks the signature even if the send request returns an error. Refresh with `import --signature` after a timeout or when transaction metadata is delayed.

```sh
cargo run --locked -- quote --input-mint "$IN_MINT" --output-mint "$OUT_MINT" --input-raw "$IN_RAW" --output-raw "$QUOTED_OUT_RAW" --output quote.json
cargo run --locked -- watch --wallet "$WALLET" --signed-tx swap.signed.b64 --quote quote.json --submit
cargo run --locked -- report --trades
```

`response_ms` measures the time until the RPC send call completes. `confirm_ms` measures the time from the start of that send call to the first local observation of `confirmed` or `finalized`, including polling delay. A timed-out observation can later resolve on-chain. Refreshing it leaves the original confirmation time unknown.

## What the numbers represent

- SPL fills are the wallet's net balance changes by mint. Intermediate assets with zero net change drop out. SOL estimates aggregate wallet and owned token-account lamports and add back the fee when the wallet is the fee payer. Other SOL movements in the transaction can affect that estimate. Successful transactions without a clean pair of balance deltas retain their chain outcome with blank fill fields.
- History scans select transactions that reference the configured programs in top-level or recorded inner instructions. This is a program filter, so its coverage depends on the selected programs and may include other operations by those programs. Failed entries without a saved quote have an unknown pair.
- ExactIn slippage is `(quoted_out - actual_out) / quoted_out × 10,000` when both mints and the actual input amount match the quote. Positive values are worse than quoted. Historical imports lack the original quote and send timestamp, so those fields remain blank.
- `chain_success_pct` uses resolved chain outcomes. The report lists unresolved entries, timeouts, and send errors separately. Confirmation percentiles use successful submissions with a locally measured time. Percentiles use nearest rank and include sample counts.
- The daily network fee sum includes fees paid by the tracked wallet. Raw token amounts and decimals are separate columns so token quantities can be reconstructed without floating-point rounding.

## Checks

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

See Solana's [transaction metadata](https://solana.com/docs/rpc/http/gettransaction), [submission](https://solana.com/docs/rpc/http/sendtransaction), and [signature status](https://solana.com/docs/rpc/http/getsignaturestatuses) reference for the RPC fields used here.
