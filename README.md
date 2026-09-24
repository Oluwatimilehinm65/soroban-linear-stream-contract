# dripswave-stream

A small, self-contained **Soroban** smart contract implementing linear
token streaming (a.k.a. vesting / payment streaming): a sender escrows a
lump sum for a recipient, and the recipient's claimable balance grows
continuously and linearly between a `start_time` and `end_time`. No
external keeper, cron job, or oracle is required. Vesting is computed
lazily from ledger timestamps whenever `withdrawable`, `withdraw`, or
`cancel` is called.

Built as a compact, well-tested reference contract ahead of the
**Dripswave** sprint.

## Design

| Function | Auth required | What it does |
|---|---|---|
| `create_stream(sender, recipient, token, deposit, start_time, end_time)` | `sender` | Escrows `deposit` of `token` from `sender` into the contract and opens a new stream. Returns the stream's `u32` id. |
| `withdrawable(stream_id) -> i128` | — (read-only) | Vested-but-unclaimed balance as of the current ledger time. |
| `withdraw(stream_id, amount)` | `recipient` | Pays `amount` (≤ `withdrawable`) to the recipient. |
| `cancel(stream_id)` | `sender` | Ends the stream immediately: whatever had vested (minus what's already been withdrawn) goes to the recipient, the rest is refunded to the sender. Freezes vesting at the cancellation timestamp. |
| `get_stream(stream_id) -> Stream` | — (read-only) | Full stored state for a stream. |

Vesting math is a single pure function (`vested_amount`) shared by every
entry point, so `withdrawable`, `withdraw`, and `cancel` can never
disagree with each other. It floors (never rounds up), so the contract
can never pay out more than has strictly vested; any rounding dust
either finishes vesting naturally or is refunded to the sender on
cancellation.

Errors are a typed `StreamError` enum (`InvalidTimeRange`,
`InvalidDeposit`, `StreamNotFound`, `AlreadyCanceled`,
`InsufficientVestedBalance`, `InvalidWithdrawAmount`) rather than raw
panics, so callers and other contracts can match on them.

Three events are published — `StreamCreated`, `StreamWithdrawn`,
`StreamCanceled` — each topic-indexed by `stream_id`.

## Project layout

```
dripswave-stream/
├── Cargo.toml
├── src/
│   ├── lib.rs               # the contract
│   └── test.rs              # the test suite (see below)
└── README.md
```

There is deliberately no `.cargo/config.toml` forcing a default build
target. `cargo test` should run natively on your host machine — only the
final deployable binary needs an explicit WASM target (see Build below).

## Test suite

`src/test.rs` covers, in isolation:

- **Creation**: state is stored correctly, funds are escrowed, invalid
  time ranges and non-positive deposits are rejected, independent
  streams don't leak into each other's balances.
- **Vesting math**: zero before `start_time`, linear in between, full at
  and after `end_time`, and floor-not-ceiling rounding on an
  awkward (non-divisible) duration.
- **Withdrawals**: partial withdrawals, repeated partial withdrawals
  that accumulate correctly, withdrawing the full vested amount,
  over-withdrawal being rejected, zero-amount withdrawals being
  rejected, and withdrawing against a stream id that doesn't exist.
- **Cancellation**: cancel before `start_time` (full refund), mid-stream
  (correct vested/unvested split), after a partial withdrawal has
  already happened, after `end_time` (recipient gets everything),
  double-cancellation being rejected, and cancelling a nonexistent
  stream.
- **Authorization boundary**: `create_stream`, `withdraw`, and `cancel`
  each panic when called without the required party's real
  authorization — tested with the auth mock deliberately turned *off*
  for the call under test, so it's the host's own enforcement being
  exercised, not application logic pretending to check it.

That's ~24 tests. Run them with:

```bash
cargo test
```

Build the deployable Wasm with (requires Rust 1.84+ and `rustup target add
wasm32v1-none`):

```bash
cargo build --target wasm32v1-none --release
```

or, if you have the Stellar CLI installed, simply `stellar contract build`,
which does the same thing and is the officially recommended path.

## Caveats / before you push this

This was written in an environment without a Rust toolchain or network
access, so **it has not been compiled or run here** — please run
`cargo test` locally before relying on it. Spots most likely to need a
small tweak depending on your exact `soroban-sdk` version:

- `env.register_stellar_asset_contract_v2(admin)` — the Stellar Asset
  Contract test-helper name/signature has shifted across SDK releases;
  if it doesn't resolve, check `soroban_sdk::testutils` for the current
  name in your resolved version.
- `env.register(DripswaveStream, ())` — the zero-arg contract
  registration helper; older SDKs used `env.register_contract(None, DripswaveStream)`.
- **Toolchain**: Soroban contracts currently need Rust 1.84+ and the
  `wasm32v1-none` target (`rustup target add wasm32v1-none`) to build the
  deployable binary. `cargo test` does not need this — it runs on your
  host toolchain like any other Rust crate, as long as nothing forces a
  default build target in `.cargo/config.toml`.

Everything else (contract logic, storage layout, error types, events,
vesting math) is version-stable Soroban SDK usage.
