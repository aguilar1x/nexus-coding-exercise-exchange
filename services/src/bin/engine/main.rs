//! Matching engine runner.
//!
//! All I/O lives here: the poll loop, the cursor, and printing. The engine it
//! feeds is a pure fold, which is what keeps the matching logic testable
//! without a server.
//!
//! Cursor discipline: `GET /orders?since=N` returns messages with an id
//! strictly greater than `N`, the feed retains full history, and ids are
//! assigned under the same lock as the push. So polling cannot gap or
//! duplicate, and restarting with `--from 0` replays identical state.

mod tui;

use clap::Parser;
use services::engine::cursor::Cursor;
use services::engine::matcher::Engine;
use services::engine::types::{Event, Price, Qty, Symbol, Trade, resolve_symbol};
use services::feed::Side as FeedSide;
use services::feed::{OrderId, OrderMessage, Side};
use std::time::Duration;
use tokio::time::sleep;

const MAX_BACKOFF_MS: u64 = 5_000;

/// How many silent polls before we suspect the feed restarted. At the default
/// 200ms poll that is about a second, so the probe costs nothing in a live
/// market and fires quickly in a dead one.
const EMPTY_POLLS_BEFORE_PROBE: u32 = 5;

/// Price levels shown per side in the snapshot.
const DEPTH_LEVELS: usize = 3;

#[derive(Parser, Debug)]
#[command(version, about = "Consumes the exchange order feed and matches it")]
struct Args {
    /// Base URL of the running order feed.
    #[arg(long, default_value = "http://127.0.0.1:3000")]
    url: String,

    /// How long to wait between polls, in milliseconds.
    #[arg(long, default_value_t = 200)]
    poll_ms: u64,

    /// Only match this symbol. Omit to match all of them.
    #[arg(long)]
    symbol: Option<String>,

    /// Feed id to start from. 0 replays the entire history.
    #[arg(long, default_value_t = 0)]
    from: OrderId,

    /// Print a market snapshot every N messages. 0 disables it.
    #[arg(long, default_value_t = 25)]
    snapshot_every: u64,

    /// Suppress the per-message rows and show only trades and snapshots.
    #[arg(long)]
    quiet: bool,

    /// Draw a live terminal UI instead of printing rows. The plain renderer
    /// stays the default because it is the one you can pipe to a file.
    #[arg(long)]
    ui: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // Validate the filter up front rather than silently matching nothing.
    let only: Option<Symbol> = match &args.symbol {
        Some(s) => match resolve_symbol(s) {
            Some(sym) => Some(sym),
            None => {
                eprintln!("error: '{}' is not a valid symbol", s);
                std::process::exit(2);
            }
        },
        None => None,
    };

    // One client reused across every poll, so connections are pooled. The feed
    // sets no timeout of its own, so we set one here.
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: could not build HTTP client: {}", e);
            std::process::exit(1);
        }
    };

    if !args.ui {
        println!("engine -> {}  (poll {}ms)", args.url, args.poll_ms);
        println!(
            "symbol filter: {}",
            args.symbol.as_deref().unwrap_or("none (all symbols)")
        );
        println!(
            "starting cursor: since={}   (ctrl-c for a final summary)",
            args.from
        );
        println!("{}", "=".repeat(84));
    }

    let mut engine = build_engine(only);
    let mut cursor = Cursor::new(args.from, EMPTY_POLLS_BEFORE_PROBE);
    let mut terminal = if args.ui {
        match tui::enter() {
            Ok(term) => Some(term),
            Err(e) => {
                eprintln!("error: --ui needs a terminal, but none is attached ({})", e);
                eprintln!("       drop --ui to use the plain renderer, which pipes to a file.");
                std::process::exit(2);
            }
        }
    } else {
        None
    };
    let mut tape = tui::TapeBuffer::new(tui::TAPE_CAPACITY);
    let mut skipped: u64 = 0;
    let mut backoff_ms = args.poll_ms;
    let mut disconnected = false;

    loop {
        let url = format!("{}/orders?since={}", args.url, cursor.since());
        // How long to wait after this iteration: the normal poll interval, or
        // the backoff when the feed is unreachable.
        let mut wait_ms = args.poll_ms;

        match fetch(&client, &url).await {
            Ok(messages) => {
                if disconnected {
                    if terminal.is_none() {
                        println!("-- reconnected to {} --", args.url);
                    }
                    disconnected = false;
                }
                backoff_ms = args.poll_ms;

                // A feed restart resets ids to 1, leaving our cursor
                // permanently ahead of it: `since` matches nothing and the
                // engine goes silently deaf. Detect it and rebuild.
                if messages.is_empty() && cursor.note_empty_poll() {
                    let max_id = feed_max_id(&client, &args.url).await;
                    if cursor.detects_restart(max_id) && terminal.is_none() {
                        println!(
                            "!! feed restarted (its latest id is {}, cursor was {}) \
                             -- discarding stale books and replaying from 0",
                            max_id.unwrap_or(0),
                            cursor.since()
                        );
                        engine = build_engine(only);
                        cursor.rewind();
                        skipped = 0;
                    } else if cursor.detects_restart(max_id) {
                        engine = build_engine(only);
                        cursor.rewind();
                        skipped = 0;
                        tape = tui::TapeBuffer::new(tui::TAPE_CAPACITY);
                    }
                }

                for msg in messages {
                    // Advance the cursor first: even a message we skip has been
                    // consumed, and re-reading it would loop forever.
                    cursor.advance_to(msg.id());

                    let event = match Event::try_from(msg) {
                        Ok(ev) => ev,
                        Err(e) => {
                            // A malformed message is skipped, never fatal.
                            skipped += 1;
                            if terminal.is_none() {
                                eprintln!("#{:>6} | SKIP   | {}", cursor.since(), e);
                            }
                            continue;
                        }
                    };

                    if terminal.is_none() && !args.quiet {
                        print_message(&engine, &event);
                    }
                    for trade in engine.apply(&event) {
                        if terminal.is_some() {
                            tape.push(tui::TapeEntry {
                                seq: trade.seq,
                                symbol: trade.symbol,
                                price: trade.price,
                                qty: trade.qty,
                                taker_bought: trade.taker_side == FeedSide::Buy,
                            });
                        } else {
                            print_trade(&trade);
                        }
                    }

                    if terminal.is_none()
                        && args.snapshot_every > 0
                        && engine.stats.events.is_multiple_of(args.snapshot_every)
                    {
                        print_snapshot(&engine, cursor.since(), skipped);
                    }
                }
            }
            Err(e) => {
                if !disconnected {
                    if terminal.is_none() {
                        eprintln!("-- feed unreachable ({}), retrying --", e);
                    }
                    disconnected = true;
                }
                // Back off, but fall through: the UI must still redraw so it can
                // show RECONNECTING and still answer the quit key. Skipping
                // straight to the next poll leaves a blank screen that looks
                // like a hang.
                wait_ms = backoff_ms;
                backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
            }
        }

        if let Some(term) = terminal.as_mut() {
            let view = tui::View {
                engine: &engine,
                tape: &tape,
                url: &args.url,
                cursor: cursor.since(),
                connected: !disconnected,
                filter: only,
                skipped,
            };
            if term.draw(|frame| tui::render(frame, &view)).is_err() {
                break;
            }
            if tui::quit_requested().unwrap_or(false) {
                break;
            }
        }

        if interruptible_sleep(wait_ms).await {
            break;
        }
    }

    // Leave the alternate screen before printing, or the summary is drawn onto
    // a screen that is about to be discarded.
    if terminal.take().is_some() {
        tui::leave();
    }
    println!("\n-- interrupted, final state --");
    print_snapshot(&engine, cursor.since(), skipped);
}

fn build_engine(only: Option<Symbol>) -> Engine {
    match only {
        Some(symbol) => Engine::only(symbol),
        None => Engine::new(),
    }
}

/// Sleeps, returning true if the user interrupted instead.
async fn interruptible_sleep(ms: u64) -> bool {
    tokio::select! {
        _ = sleep(Duration::from_millis(ms)) => false,
        _ = tokio::signal::ctrl_c() => true,
    }
}

/// One poll. Any failure is reported as a string so the caller can back off
/// instead of aborting.
async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<OrderMessage>, String> {
    let res = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }
    res.json::<Vec<OrderMessage>>()
        .await
        .map_err(|e| format!("decode: {}", e))
}

/// The feed's newest message id, used only to detect a restart.
async fn feed_max_id(client: &reqwest::Client, base: &str) -> Option<OrderId> {
    let url = format!("{}/orders?n=1", base);
    let messages = fetch(client, &url).await.ok()?;
    messages.last().map(|m| m.id())
}

/// One row per feed message, printed before the event is applied.
fn print_message(engine: &Engine, event: &Event) {
    match event {
        Event::New { symbol, order } => println!(
            "#{:>6} | NEW    | {:<8} | acct {:>3} | {:<4} | {:>9} x {:>5}",
            order.id,
            symbol,
            order.account,
            side_label(order.side),
            order.price,
            order.qty,
        ),
        Event::Cancel {
            seq,
            account,
            target_id,
        } => {
            // The wire message carries no symbol, so it is resolved here from
            // the engine's routing index — before `apply` consumes the entry.
            let (symbol, outcome) = match engine.symbol_of(*target_id) {
                Some(s) => (s, "hit"),
                None => ("?", "unknown"),
            };
            println!(
                "#{:>6} | CANCEL | {:<8} | acct {:>3} | -> #{:<6} {:>16}",
                seq,
                symbol,
                account,
                target_id,
                format!("[{}]", outcome),
            );
        }
    }
}

/// Trades are indented under the message that caused them.
fn print_trade(t: &Trade) {
    println!(
        "        +- TRADE | {:<8} | maker #{} a{} <- taker #{} a{} | {} x {}",
        t.symbol, t.maker_order, t.maker_account, t.taker_order, t.taker_account, t.price, t.qty,
    );
}

fn side_label(side: Side) -> &'static str {
    match side {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    }
}

fn print_snapshot(engine: &Engine, cursor: OrderId, skipped: u64) {
    let s = &engine.stats;
    println!("{}", "-".repeat(84));
    println!(
        "cursor={} msgs={} new={} filtered={} cancels={} (hit {} / unknown {}) skipped={}",
        cursor,
        s.events,
        s.new_orders,
        s.filtered,
        s.cancels,
        s.cancels_hit,
        s.cancels_unknown,
        skipped,
    );
    println!(
        "trades={} volume={} cancelled={} resting={} indexed={} net_position={}",
        s.trades,
        Qty(s.volume),
        Qty(s.cancelled_qty),
        engine.resting(),
        engine.indexed(),
        Qty(engine.market.net_position()),
    );

    let mut symbols: Vec<Symbol> = engine.symbols().copied().collect();
    symbols.sort_unstable();
    for symbol in symbols {
        let Some(book) = engine.book(symbol) else {
            continue;
        };

        match engine.market.symbol(symbol) {
            Some(m) => println!(
                "  {:<8}  last {:>9}  vwap {:>9}  hi {:>9}  lo {:>9}  vol {:>7} ({} trades)",
                symbol,
                fmt_price(m.last),
                fmt_price(m.vwap()),
                fmt_price(m.high),
                fmt_price(m.low),
                Qty(m.volume),
                m.trades,
            ),
            None => println!("  {:<8}  (no trades yet)", symbol),
        }

        let (bids, asks) = book.depth(DEPTH_LEVELS);
        for i in 0..DEPTH_LEVELS.min(bids.len().max(asks.len())) {
            println!(
                "            {:>11} | {:<11}",
                bids.get(i)
                    .map(|(p, q)| format!("{} x {}", p, q))
                    .unwrap_or_default(),
                asks.get(i)
                    .map(|(p, q)| format!("{} x {}", p, q))
                    .unwrap_or_default(),
            );
        }
    }

    let top = engine.market.top_positions(4);
    if !top.is_empty() {
        let line: Vec<String> = top
            .iter()
            .map(|(account, symbol, qty)| {
                // `Qty` keeps the sign and the decimal; formatting through f64
                // here would print "-61" instead of "-61.0".
                let sign = if qty.0 > 0 { "+" } else { "" };
                format!("a{} {} {}{}", account, symbol, sign, qty)
            })
            .collect();
        println!("  positions: {}", line.join("  |  "));
    }

    // The core invariants: matching leaves no book crossed, the routing index
    // tracks the books exactly, and every unit bought was sold by someone.
    if engine.any_crossed() {
        eprintln!("  !! INVARIANT VIOLATED: a book is crossed after matching");
    }
    if engine.indexed() != engine.resting() {
        eprintln!("  !! INVARIANT VIOLATED: routing index drifted from the books");
    }
    if engine.market.net_position() != 0 {
        eprintln!("  !! INVARIANT VIOLATED: positions do not net to zero");
    }
    println!("{}", "-".repeat(84));
}

fn fmt_price(p: Option<Price>) -> String {
    p.map(|p| p.to_string()).unwrap_or_else(|| "-".to_string())
}
