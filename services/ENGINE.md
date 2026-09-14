# Matching Engine

The feed in [`src/feed.rs`](src/feed.rs) publishes limit orders and cancels but
**never matches them** — there are no trade messages on the wire, ever. This is
the missing half: a consumer that reads the feed, maintains an order book per
symbol, and infers the trades the exchange would have printed.

```bash
cargo test                                   # 77 tests, no server needed
```

Terminal 1 — the feed:

```bash
cargo run -- --start-feed --num-accounts 20
```

Terminal 2 — the engine, either as scrolling rows:

```bash
cargo run --bin engine
```

...or as a live terminal UI:

```bash
cargo run --bin engine -- --ui
```

**The `--` is required.** Without it cargo takes `--ui` as its own flag and the
engine never sees it — you get the plain renderer and no error. Everything after
`--` goes to the binary.

Other flags: `--symbol ETH-USDC` (one pair only), `--from 0` (replay all
history), `--quiet` (trades and snapshots only), `--poll-ms`, `--snapshot-every`,
`--url`. Ctrl-C prints a final summary.

`Cargo.toml` sets `default-run = "services"`, so a bare `cargo run` still means
the feed and the CLI examples in [README.md](README.md) keep working now that the
package has two binaries.

## Terminal UI (`--ui`)

```
┌ engine ────────────────────────────────────────────────────────────────────┐
│ http://127.0.0.1:3000  connected   cursor 3333   filter all     [q] quit    │
└────────────────────────────────────────────────────────────────────────────┘
┌ BTC-USDC ───────────┐┌ ETH-USDC ───────────┐┌ trades (32) ─────────────────┐
│ last 974.41  vwap … ││ last 102.89  vwap … ││ #3041 ETH-USDC  102.89 x 3.1 B│
│ hi … lo … vol … (n) ││ hi … lo … vol … (n) ││ #3038 NEX-USDC    9.71 x 1.5 S│
│     BIDS  |  ASKS   ││     BIDS  |  ASKS   ││ #3036 ETH-USDC  102.88 x 2.0 B│
│ 974.41 x1.0|983.03… ││ 102.83 x2.0|102.87… ││ …                             │
│ 973.15 x4.1|983.34… ││ 102.51 x5.9|102.91… ││                               │
│ 50 resting          ││ 23 resting          ││                               │
└─────────────────────┘└─────────────────────┘└───────────────────────────────┘
┌────────────────────────────────────────────────────────────────────────────┐
│ msgs 3333  new 2822  filtered 0  cancels 511 (hit 231 / unknown 280)       │
│ trades 2262  volume 6329.6  cancelled 1524.9  resting 304  indexed 304 …   │
│ positions: a13 NEX-USDC -70.9  |  a1 ETH-USDC +68.9  |  a9 BTC-USDC +66.1  │
│ invariants ok                                                              │
└────────────────────────────────────────────────────────────────────────────┘
```

Bids are green and asks red; a tape row is marked `B` or `S` by the aggressor's
side. The footer carries the same invariant checks as the plain renderer, where
they cannot scroll out of sight: a crossed book, a drifted routing index, or
positions that stop netting to zero turn that last line red.

**Keys:** `q`, `Q` or `Esc` to quit; Ctrl-C also works and prints the final
summary after restoring the screen.

**Narrow terminals:** below 100 columns the per-symbol panels stack vertically
instead of sitting side by side. It renders down to a 1x1 terminal without
panicking — there is a test for exactly that.

**No terminal attached:** `--ui` piped to a file or run under CI exits with a
message pointing at the plain renderer, rather than the raw OS error `ratatui`
would otherwise panic with.

The plain renderer stays the default on purpose: it is the one you can pipe to a
file, and its scrolling rows are the evidence that the feed is being read
correctly.

## Structure

```
bin/engine/main.rs  poll loop, HTTP, printing, CLI     <- all I/O lives here
bin/engine/tui.rs   ratatui terminal interface (--ui)
engine/types.rs     OrderMessage -> Event: f64 -> ticks, symbol validation
engine/matcher.rs   Engine::apply(&Event) -> Vec<Trade>  <- pure fold, no I/O
engine/book.rs      OrderBook: price-time priority, insert / cancel / cross
engine/market.rs    derived tape: last, VWAP, hi/lo, volume, net positions
engine/cursor.rs    stream position and feed-restart detection
```

`feed.rs` and `main.rs` are unchanged. The engine imports `OrderMessage`,
`Side`, `SYMBOLS` and `OrderMessage::id()` from the feed rather than
redefining them.

The matcher touches no network and no clock, so the whole engine is driven from
a canned list of events in tests. That is the point: correctness is checked, not
assumed.

## Decisions

**Integer ticks, not `f64`.** Prices are cents (`Price(i64)`), quantities tenths
(`Qty(i64)`), converted once at the deserialization boundary. The feed already
rounds to 2 and 1 decimals, so nothing is lost. In `f64`, a partial fill leaves
`5.0 - 3.1 = 1.9000000000000004`, the order never compares equal to zero, and it
stays wedged in the book forever. This is the single decision that most affects
correctness.

**Price-time priority.** `BTreeMap` keyed by price gives price priority for free
(best bid is the last key, best ask the first); a `VecDeque` per level gives time
priority. Because feed ids are strictly increasing, arrival order *is* id order —
there is no separate sequence number.

**The aggressor takes the resting price.** An incoming buy at 101.00 that crosses
a resting ask at 99.10 trades at **99.10**. The taker never improves on the
maker's terms, and it sweeps the best prices first.

**Self-trades are allowed.** The generator assigns accounts at random with no
economic meaning, so preventing self-trades would distort the book for nothing.
This is a choice, not an omission.

**A cancel for an unknown or already-filled order is a silent no-op.** Roughly
15% of feed messages are cancels drawn from a 50-order window, and many target
orders the engine already matched away — on real traffic, more than half of them
miss. It is counted, never logged as an error.

**Cancel ownership is recorded but not enforced.** `POST /cancel` never checks
that the account owns the target, so neither do we.

## Cancels carry no symbol

`OrderMessage::Cancel` is `{id, timestamp, account, target_id}`. With three books
that is a routing problem, and it cannot be solved during deserialization:
`TryFrom<OrderMessage>` is stateless, while knowing which book holds order 42
depends on accumulated engine state. So the engine keeps one
`HashMap<OrderId, Symbol>` — the only cross-book state in the design — and
resolves the symbol inside `apply`.

Its lifecycle is three lines, and `indexed() == resting()` is asserted after
every event to prove it does not leak:

| When | Action |
|---|---|
| an order rests | insert |
| a resting order is fully filled | remove |
| a cancel hits | remove |

An order that fills completely on arrival never rests, so it is never indexed,
and a later cancel for it correctly misses instead of mis-routing.

## Cursor and recovery

`GET /orders?since=N` returns ids strictly greater than `N`. Ids are assigned
under the same lock as the push, and the feed retains full history, so polling
**cannot gap or duplicate** and `--from 0` rebuilds identical state. That is why
there is no persistence: replay is deterministic and free, and a stored book
could only drift.

One failure mode does need handling. Restarting the feed resets its ids to 1,
leaving the cursor permanently ahead of it: `since` matches nothing and the
engine goes *silently* deaf — no error, just an empty terminal. After five empty
polls the runner checks the feed's newest id, and if it is below the cursor it
discards the stale books and replays from 0.

That decision lives in [`engine/cursor.rs`](src/engine/cursor.rs) rather than in
the poll loop, so it is unit-tested without a server. An empty feed is
deliberately *not* treated as a restart: a freshly booted feed and an unreachable
one look identical from here, and rewinding on that would be a guess.

## Invariants

Checked after every event in the replay test, and re-checked in every live
snapshot:

- no book is ever crossed once `apply` returns
- the routing index size equals the resting order count
- net position across all accounts is zero — every unit bought was sold
- `submitted == resting + 2 x traded + cancelled`

That last one initially failed against real traffic: the first version ignored
quantity that leaves the book via cancel. `OrderBook::cancel` now returns the
removed order so the unfilled remainder is accounted for.

## Tests

77 total. 63 unit tests build events by hand, 7 cover the UI (tape window plus
frames rendered against ratatui's `TestBackend`, down to a 1x1 terminal, with no
tty involved); 7 integration tests in
[`tests/replay.rs`](tests/replay.rs) fold 600 messages captured from a running
feed and assert the invariants at every step, plus determinism (two replays
produce identical state) and resumability (stopping and resuming matches a
straight run).

`cargo clippy --all-targets` is clean and the engine sources are rustfmt-clean.
`feed.rs` and `main.rs` are byte-for-byte unchanged, and are excluded from
formatting for that reason.

## Not built

Persistence (replay makes it redundant), balances (the feed has no deposits or
settlement — net positions *are* tracked, since those are derivable from the
tape), a REST API on the engine (nothing would consume it), self-trade
prevention (a decision, above), order modification (no amend message exists),
and streaming (the feed is pull-only; adding it means changing `feed.rs`).

`ratatui` is a dependency of the binary only. The library never links it, so the
matcher and its tests neither need nor touch a terminal — which is why `tui.rs`
lives under `src/bin/engine/` rather than in `src/engine/`.
