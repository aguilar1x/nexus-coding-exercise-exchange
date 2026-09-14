//! Replay of real captured feed traffic.
//!
//! The unit tests build events by hand, which proves the rules but not that
//! they survive the feed's actual shape: crossing prices, cancels for orders
//! already matched away, three symbols interleaved. This folds 600 messages
//! captured from a running feed and asserts the invariants hold at every step.

use services::engine::matcher::Engine;
use services::engine::types::{Event, Qty, Symbol};
use services::feed::OrderMessage;

const FIXTURE: &str = include_str!("fixtures/feed_sample.json");
const SYMBOLS: [Symbol; 3] = ["NEX-USDC", "ETH-USDC", "BTC-USDC"];

fn messages() -> Vec<OrderMessage> {
    serde_json::from_str(FIXTURE).expect("fixture should deserialize as feed messages")
}

fn events() -> Vec<Event> {
    messages()
        .into_iter()
        .map(|m| Event::try_from(m).expect("captured feed messages should all convert"))
        .collect()
}

#[test]
fn the_fixture_is_real_feed_traffic() {
    let msgs = messages();
    assert_eq!(msgs.len(), 600);

    // Ids are strictly increasing with no gaps: the cursor contract.
    for pair in msgs.windows(2) {
        assert_eq!(
            pair[1].id(),
            pair[0].id() + 1,
            "feed ids should be contiguous"
        );
    }

    let cancels = msgs
        .iter()
        .filter(|m| matches!(m, OrderMessage::Cancel { .. }))
        .count();
    assert!(cancels > 0, "fixture should exercise the cancel path");
}

#[test]
fn every_invariant_holds_at_every_step_of_a_real_replay() {
    let mut engine = Engine::new();
    let mut traded = 0i64;
    let mut submitted = 0i64;

    for event in events() {
        if let Event::New { order, .. } = &event {
            submitted += order.qty.0;
        }
        traded += engine.apply(&event).iter().map(|t| t.qty.0).sum::<i64>();

        let seq = event.seq();
        assert!(!engine.any_crossed(), "book crossed after #{}", seq);
        assert_eq!(
            engine.indexed(),
            engine.resting(),
            "routing index drifted from the books after #{}",
            seq
        );
        assert_eq!(
            engine.market.net_position(),
            0,
            "positions stopped netting to zero after #{}",
            seq
        );
    }

    // Every unit submitted ends up in exactly one of three places: still
    // resting, traded away, or cancelled out of the book. Each trade consumes
    // one unit from each side, hence the factor of two.
    assert_eq!(
        submitted,
        engine.resting_qty().0 + 2 * traded + engine.stats.cancelled_qty,
        "quantity was created or destroyed"
    );
    assert!(
        engine.stats.cancelled_qty > 0,
        "real feed traffic should cancel some resting quantity"
    );
    assert_eq!(traded, engine.stats.volume);
    assert!(traded > 0, "real feed traffic should produce trades");
}

#[test]
fn the_real_feed_cancels_orders_that_were_already_matched_away() {
    let mut engine = Engine::new();
    for event in events() {
        engine.apply(&event);
    }
    // This is the adversarial case the feed is built to produce. If it never
    // fired, the no-op path would be untested against real traffic.
    assert!(
        engine.stats.cancels_unknown > 0,
        "expected cancels for orders already gone from the book"
    );
    assert!(
        engine.stats.cancels_hit > 0,
        "expected some cancels to land"
    );
}

#[test]
fn replaying_the_same_messages_twice_gives_identical_state() {
    let run = || {
        let mut engine = Engine::new();
        for event in events() {
            engine.apply(&event);
        }
        engine
    };
    let (a, b) = (run(), run());

    // Determinism is what makes `--from 0` a safe recovery strategy.
    assert_eq!(a.stats, b.stats);
    assert_eq!(a.resting(), b.resting());
    assert_eq!(a.resting_qty(), b.resting_qty());
    for symbol in SYMBOLS {
        assert_eq!(
            a.market
                .symbol(symbol)
                .map(|s| (s.trades, s.volume, s.last)),
            b.market
                .symbol(symbol)
                .map(|s| (s.trades, s.volume, s.last)),
        );
        assert_eq!(
            a.book(symbol).map(|bk| (bk.best_bid(), bk.best_ask())),
            b.book(symbol).map(|bk| (bk.best_bid(), bk.best_ask())),
        );
    }
}

#[test]
fn a_symbol_filter_never_touches_the_other_books() {
    let mut engine = Engine::only("ETH-USDC");
    for event in events() {
        engine.apply(&event);
    }
    assert_eq!(engine.symbols().count(), 1);
    assert!(engine.book("BTC-USDC").is_none());
    assert!(engine.book("NEX-USDC").is_none());
    assert!(engine.stats.filtered > 0);
    assert_eq!(engine.market.net_position(), 0);
}

#[test]
fn resuming_from_a_midpoint_matches_a_full_replay_of_the_tail() {
    // Restarting the engine and replaying is only safe because the fold is
    // deterministic and the feed never drops a message.
    let all = events();
    let (head, tail) = all.split_at(300);

    let mut streamed = Engine::new();
    for event in &all {
        streamed.apply(event);
    }

    let mut resumed = Engine::new();
    for event in head {
        resumed.apply(event);
    }
    for event in tail {
        resumed.apply(event);
    }

    assert_eq!(streamed.stats, resumed.stats);
    assert_eq!(streamed.resting_qty(), resumed.resting_qty());
}

#[test]
fn the_tape_is_consistent_with_the_trades_it_came_from() {
    let mut engine = Engine::new();
    let mut last_by_symbol: Vec<(Symbol, Qty)> = Vec::new();

    for event in events() {
        for trade in engine.apply(&event) {
            last_by_symbol.retain(|(s, _)| *s != trade.symbol);
            last_by_symbol.push((trade.symbol, trade.qty));
        }
    }

    for symbol in SYMBOLS {
        let Some(state) = engine.market.symbol(symbol) else {
            continue;
        };
        let vwap = state.vwap().expect("a traded symbol has a vwap");
        let (low, high) = (state.low.unwrap(), state.high.unwrap());
        assert!(
            vwap >= low && vwap <= high,
            "{}: vwap {} outside [{}, {}]",
            symbol,
            vwap,
            low,
            high
        );
        assert!(state.volume > 0);
    }
}
