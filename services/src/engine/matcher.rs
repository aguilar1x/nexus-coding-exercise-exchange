//! The matching engine: a pure fold over the feed's message sequence.
//!
//! `apply` touches no network and no clock, so the entire engine can be driven
//! from a canned list of events with no server running. That is what makes
//! correctness checkable rather than merely plausible.
//!
//! Semantics, all deliberate:
//!   * price-time priority — best price first, then earliest arrival
//!   * the aggressor takes the **resting** order's price, never its own
//!   * self-trades are allowed; the feed has no account model to protect
//!   * a cancel for an unknown or already-filled order is a silent no-op
//!   * cancel ownership is recorded but never enforced

use crate::engine::book::OrderBook;
use crate::engine::market::MarketState;
use crate::engine::types::{Event, Order, Qty, Symbol, Trade};
use crate::feed::{OrderId, Side};
use std::collections::HashMap;

/// Counters over everything the engine has folded.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    pub events: u64,
    pub new_orders: u64,
    pub filtered: u64,
    pub cancels: u64,
    pub cancels_hit: u64,
    pub cancels_unknown: u64,
    pub trades: u64,
    /// Traded quantity in ticks (tenths).
    pub volume: i64,
    /// Quantity that left the book via cancel without ever trading. Needed to
    /// account for every unit submitted: a unit is resting, traded, or cancelled.
    pub cancelled_qty: i64,
}

#[derive(Debug, Default)]
pub struct Engine {
    books: HashMap<Symbol, OrderBook>,
    /// Which book holds each resting order.
    ///
    /// This exists because `OrderMessage::Cancel` carries no symbol, so a
    /// cancel cannot be routed from the message alone. It is the only piece of
    /// cross-book state in the engine.
    order_symbol: HashMap<OrderId, Symbol>,
    /// When set, every other symbol is ignored. Used to land matching on one
    /// symbol before opening it up to all of them.
    only: Option<Symbol>,
    pub stats: Stats,
    /// Derived tape state: last/high/low, VWAP, volume and net positions.
    /// Fed only from trades, never from the book.
    pub market: MarketState,
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    /// An engine that processes only `symbol`.
    pub fn only(symbol: Symbol) -> Self {
        Engine {
            only: Some(symbol),
            ..Default::default()
        }
    }

    /// Folds one event into the books, returning any trades it caused.
    /// A cancel never produces a trade.
    pub fn apply(&mut self, event: &Event) -> Vec<Trade> {
        self.stats.events += 1;
        match event {
            Event::New { symbol, order } => self.apply_new(symbol, *order),
            Event::Cancel { target_id, .. } => {
                self.apply_cancel(*target_id);
                Vec::new()
            }
        }
    }

    fn apply_new(&mut self, symbol: Symbol, incoming: Order) -> Vec<Trade> {
        if let Some(filter) = self.only
            && symbol != filter
        {
            self.stats.filtered += 1;
            return Vec::new();
        }
        self.stats.new_orders += 1;

        let mut taker = incoming;
        let opposite = opposite(taker.side);
        let book = self.books.entry(symbol).or_default();
        let mut trades = Vec::new();

        // Walk the opposite side while the incoming order still has quantity
        // and the best resting price is one it is willing to trade at.
        while !taker.qty.is_zero() {
            let Some(best) = book.best_order(opposite) else {
                break;
            };
            let crosses = match taker.side {
                Side::Buy => best.price <= taker.price,
                Side::Sell => best.price >= taker.price,
            };
            if !crosses {
                break;
            }

            let fill = Qty(taker.qty.0.min(best.qty.0));
            let maker = book
                .reduce_best(opposite, fill)
                .expect("a best order was just observed");
            taker.qty = Qty(taker.qty.0 - fill.0);

            // The maker left the book only if this fill exhausted it.
            if maker.qty == fill {
                self.order_symbol.remove(&maker.id);
            }

            self.stats.trades += 1;
            self.stats.volume += fill.0;
            let trade = Trade {
                seq: taker.id,
                symbol,
                // The resting order sets the price. The aggressor crossed to
                // meet it, so it never improves on the maker's terms.
                price: maker.price,
                qty: fill,
                maker_order: maker.id,
                maker_account: maker.account,
                taker_order: taker.id,
                taker_account: taker.account,
                taker_side: taker.side,
            };
            self.market.record(&trade);
            trades.push(trade);
        }

        // Whatever is left rests and becomes available to future aggressors.
        if !taker.qty.is_zero() {
            book.insert(taker);
            self.order_symbol.insert(taker.id, symbol);
        }

        trades
    }

    fn apply_cancel(&mut self, target_id: OrderId) {
        self.stats.cancels += 1;

        // The symbol is derived here, never received: it is absent from the
        // wire message and depends on state this engine accumulated.
        let Some(symbol) = self.order_symbol.get(&target_id).copied() else {
            self.stats.cancels_unknown += 1;
            return;
        };

        let removed = self
            .books
            .get_mut(symbol)
            .and_then(|book| book.cancel(target_id));

        match removed {
            Some(order) => {
                self.order_symbol.remove(&target_id);
                self.stats.cancels_hit += 1;
                self.stats.cancelled_qty += order.qty.0;
            }
            None => self.stats.cancels_unknown += 1,
        }
    }

    /// Which book holds a resting order, if it is still resting. The runner
    /// uses this to label a cancel before applying it, since the wire message
    /// carries no symbol.
    pub fn symbol_of(&self, id: OrderId) -> Option<Symbol> {
        self.order_symbol.get(&id).copied()
    }

    pub fn book(&self, symbol: Symbol) -> Option<&OrderBook> {
        self.books.get(symbol)
    }

    pub fn symbols(&self) -> impl Iterator<Item = &Symbol> {
        self.books.keys()
    }

    /// Total resting orders across every book.
    pub fn resting(&self) -> usize {
        self.books.values().map(|b| b.len()).sum()
    }

    /// Total quantity resting across every book.
    pub fn resting_qty(&self) -> Qty {
        Qty(self.books.values().map(|b| b.total_qty().0).sum())
    }

    /// Size of the cancel-routing index. Should always equal `resting()`;
    /// a drift between them means the index is leaking.
    pub fn indexed(&self) -> usize {
        self.order_symbol.len()
    }

    /// True if any book is crossed. Must be false after every `apply`.
    pub fn any_crossed(&self) -> bool {
        self.books.values().any(|b| b.is_crossed())
    }
}

fn opposite(side: Side) -> Side {
    match side {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::types::Price;

    const ETH: Symbol = "ETH-USDC";
    const BTC: Symbol = "BTC-USDC";

    fn new(id: OrderId, symbol: Symbol, account: u32, side: Side, price: i64, qty: i64) -> Event {
        Event::New {
            symbol,
            order: Order {
                id,
                account,
                side,
                price: Price(price),
                qty: Qty(qty),
            },
        }
    }

    fn cancel(seq: OrderId, account: u32, target_id: OrderId) -> Event {
        Event::Cancel {
            seq,
            account,
            target_id,
        }
    }

    #[test]
    fn an_order_with_nothing_to_cross_just_rests() {
        let mut e = Engine::new();
        let trades = e.apply(&new(1, ETH, 1, Side::Buy, 10000, 50));
        assert!(trades.is_empty());
        assert_eq!(e.resting(), 1);
        assert_eq!(e.book(ETH).unwrap().best_bid(), Some(Price(10000)));
    }

    #[test]
    fn an_exact_cross_trades_and_empties_the_book() {
        let mut e = Engine::new();
        e.apply(&new(1, ETH, 1, Side::Sell, 10000, 50));
        let trades = e.apply(&new(2, ETH, 2, Side::Buy, 10000, 50));
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].qty, Qty(50));
        assert_eq!(trades[0].maker_order, 1);
        assert_eq!(trades[0].taker_order, 2);
        assert!(e.book(ETH).unwrap().is_empty());
        assert_eq!(e.indexed(), 0);
    }

    #[test]
    fn the_aggressor_takes_the_resting_price() {
        let mut e = Engine::new();
        // Resting ask at 100.00; an aggressive buy willing to pay 101.00.
        e.apply(&new(1, ETH, 1, Side::Sell, 10000, 50));
        let trades = e.apply(&new(2, ETH, 2, Side::Buy, 10100, 50));
        assert_eq!(trades.len(), 1);
        // Trades at the maker's 100.00, not the taker's 101.00.
        assert_eq!(trades[0].price, Price(10000));
        assert_eq!(trades[0].taker_side, Side::Buy);
    }

    #[test]
    fn a_partial_fill_leaves_an_exact_remainder() {
        let mut e = Engine::new();
        e.apply(&new(1, ETH, 1, Side::Sell, 10000, 50)); // resting 5.0
        let trades = e.apply(&new(2, ETH, 2, Side::Buy, 10000, 31)); // takes 3.1
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].qty, Qty(31));
        // 5.0 - 3.1 = 1.9 exactly. Under f64 this is 1.9000000000000004 and
        // the order would never leave the book.
        let book = e.book(ETH).unwrap();
        assert_eq!(book.best_order(Side::Sell).unwrap().qty, Qty(19));
        assert_eq!(e.resting(), 1);
    }

    #[test]
    fn an_unfilled_remainder_of_the_aggressor_rests() {
        let mut e = Engine::new();
        e.apply(&new(1, ETH, 1, Side::Sell, 10000, 20));
        let trades = e.apply(&new(2, ETH, 2, Side::Buy, 10000, 50));
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].qty, Qty(20));
        // 3.0 of the buy is left over and becomes the new best bid.
        let book = e.book(ETH).unwrap();
        assert_eq!(book.best_bid(), Some(Price(10000)));
        assert_eq!(book.best_order(Side::Buy).unwrap().qty, Qty(30));
    }

    #[test]
    fn equal_prices_fill_in_arrival_order() {
        let mut e = Engine::new();
        e.apply(&new(1, ETH, 1, Side::Sell, 10000, 10));
        e.apply(&new(2, ETH, 2, Side::Sell, 10000, 10));
        let trades = e.apply(&new(3, ETH, 3, Side::Buy, 10000, 10));
        assert_eq!(trades.len(), 1);
        // Order 1 arrived first, so it fills first.
        assert_eq!(trades[0].maker_order, 1);
    }

    #[test]
    fn one_order_sweeps_several_levels_best_price_first() {
        let mut e = Engine::new();
        e.apply(&new(1, ETH, 1, Side::Sell, 10200, 10));
        e.apply(&new(2, ETH, 2, Side::Sell, 10000, 10));
        e.apply(&new(3, ETH, 3, Side::Sell, 10100, 10));
        let trades = e.apply(&new(4, ETH, 4, Side::Buy, 10200, 30));

        assert_eq!(trades.len(), 3);
        // Cheapest ask consumed first, then the next.
        assert_eq!(
            trades.iter().map(|t| t.price).collect::<Vec<_>>(),
            vec![Price(10000), Price(10100), Price(10200)]
        );
        assert_eq!(trades.iter().map(|t| t.qty.0).sum::<i64>(), 30);
        assert!(e.book(ETH).unwrap().is_empty());
    }

    #[test]
    fn a_non_crossing_order_does_not_trade() {
        let mut e = Engine::new();
        e.apply(&new(1, ETH, 1, Side::Sell, 10100, 10));
        let trades = e.apply(&new(2, ETH, 2, Side::Buy, 10000, 10));
        assert!(trades.is_empty());
        assert_eq!(e.resting(), 2);
        assert!(!e.any_crossed());
    }

    #[test]
    fn the_book_is_never_crossed_after_apply() {
        let mut e = Engine::new();
        for (i, (side, price, qty)) in [
            (Side::Buy, 10000, 50),
            (Side::Sell, 9900, 30),
            (Side::Buy, 10100, 40),
            (Side::Sell, 9800, 90),
        ]
        .into_iter()
        .enumerate()
        {
            e.apply(&new(i as u64 + 1, ETH, 1, side, price, qty));
            assert!(!e.any_crossed(), "crossed after event {}", i + 1);
        }
    }

    #[test]
    fn self_trades_are_allowed() {
        let mut e = Engine::new();
        e.apply(&new(1, ETH, 7, Side::Sell, 10000, 50));
        let trades = e.apply(&new(2, ETH, 7, Side::Buy, 10000, 50));
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].maker_account, 7);
        assert_eq!(trades[0].taker_account, 7);
    }

    #[test]
    fn a_cancel_removes_a_resting_order() {
        let mut e = Engine::new();
        e.apply(&new(1, ETH, 1, Side::Buy, 10000, 50));
        let trades = e.apply(&cancel(2, 1, 1));
        assert!(trades.is_empty());
        assert_eq!(e.resting(), 0);
        assert_eq!(e.stats.cancels_hit, 1);
        assert_eq!(e.stats.cancels_unknown, 0);
    }

    #[test]
    fn a_cancel_for_an_order_that_never_existed_is_a_no_op() {
        let mut e = Engine::new();
        e.apply(&new(1, ETH, 1, Side::Buy, 10000, 50));
        let trades = e.apply(&cancel(2, 1, 9999));
        assert!(trades.is_empty());
        assert_eq!(e.resting(), 1);
        assert_eq!(e.stats.cancels_unknown, 1);
    }

    #[test]
    fn a_cancel_for_an_already_filled_order_is_a_no_op() {
        let mut e = Engine::new();
        e.apply(&new(1, ETH, 1, Side::Sell, 10000, 50));
        e.apply(&new(2, ETH, 2, Side::Buy, 10000, 50)); // order 1 is now gone
        let trades = e.apply(&cancel(3, 1, 1));
        assert!(trades.is_empty());
        assert_eq!(e.stats.cancels_unknown, 1);
        assert_eq!(e.stats.cancels_hit, 0);
    }

    #[test]
    fn an_order_that_fills_on_arrival_is_never_indexed() {
        let mut e = Engine::new();
        e.apply(&new(1, ETH, 1, Side::Sell, 10000, 50));
        // Order 2 fills completely on arrival, so it never rests.
        e.apply(&new(2, ETH, 2, Side::Buy, 10000, 50));
        assert_eq!(e.indexed(), 0);
        // A cancel for it therefore takes the unknown branch, not a misroute.
        e.apply(&cancel(3, 2, 2));
        assert_eq!(e.stats.cancels_unknown, 1);
    }

    #[test]
    fn cancel_ownership_is_not_enforced() {
        let mut e = Engine::new();
        e.apply(&new(1, ETH, 1, Side::Buy, 10000, 50));
        // Account 99 cancels account 1's order. The feed permits this and so
        // do we; the account is recorded, never checked.
        e.apply(&cancel(2, 99, 1));
        assert_eq!(e.stats.cancels_hit, 1);
        assert_eq!(e.resting(), 0);
    }

    #[test]
    fn symbols_do_not_match_against_each_other() {
        let mut e = Engine::new();
        e.apply(&new(1, BTC, 1, Side::Sell, 10000, 50));
        let trades = e.apply(&new(2, ETH, 2, Side::Buy, 10000, 50));
        assert!(trades.is_empty(), "an ETH order matched against BTC");
        assert_eq!(e.resting(), 2);
    }

    #[test]
    fn a_cancel_routes_to_the_right_book() {
        let mut e = Engine::new();
        e.apply(&new(1, BTC, 1, Side::Buy, 10000, 50));
        e.apply(&new(2, ETH, 1, Side::Buy, 10000, 50));
        e.apply(&cancel(3, 1, 1));
        assert_eq!(e.stats.cancels_hit, 1);
        assert!(e.book(BTC).unwrap().is_empty());
        assert_eq!(e.book(ETH).unwrap().len(), 1, "cancelled the wrong book");
    }

    #[test]
    fn the_symbol_filter_ignores_other_symbols() {
        let mut e = Engine::only(ETH);
        e.apply(&new(1, BTC, 1, Side::Buy, 10000, 50));
        e.apply(&new(2, ETH, 1, Side::Buy, 10000, 50));
        assert_eq!(e.stats.filtered, 1);
        assert_eq!(e.stats.new_orders, 1);
        assert_eq!(e.resting(), 1);
        // The filtered order was never indexed, so its cancel is unknown.
        e.apply(&cancel(3, 1, 1));
        assert_eq!(e.stats.cancels_unknown, 1);
    }

    #[test]
    fn the_routing_index_never_drifts_from_the_resting_count() {
        let mut e = Engine::new();
        let events = [
            new(1, ETH, 1, Side::Buy, 10000, 50),
            new(2, ETH, 2, Side::Sell, 10000, 20),
            new(3, BTC, 3, Side::Sell, 99000, 40),
            cancel(4, 1, 1),
            new(5, ETH, 4, Side::Sell, 9900, 70),
            new(6, ETH, 5, Side::Buy, 9900, 70),
            cancel(7, 3, 3),
            cancel(8, 9, 12345),
        ];
        for ev in &events {
            e.apply(ev);
            assert_eq!(
                e.indexed(),
                e.resting(),
                "index drifted from the book after #{}",
                ev.seq()
            );
            assert!(!e.any_crossed(), "crossed after #{}", ev.seq());
        }
    }

    #[test]
    fn every_filled_unit_appears_in_exactly_one_trade() {
        let mut e = Engine::new();
        let events = [
            new(1, ETH, 1, Side::Sell, 10000, 30),
            new(2, ETH, 2, Side::Sell, 10100, 40),
            new(3, ETH, 3, Side::Buy, 10200, 100),
            new(4, ETH, 4, Side::Sell, 10000, 25),
        ];
        let mut traded = 0i64;
        let mut submitted = 0i64;
        for ev in &events {
            if let Event::New { order, .. } = ev {
                submitted += order.qty.0;
            }
            traded += e.apply(ev).iter().map(|t| t.qty.0).sum::<i64>();
        }
        // Each trade consumes one unit from each side, so submitted quantity
        // equals resting quantity plus twice the traded quantity.
        let resting: i64 = e
            .book("ETH-USDC")
            .unwrap()
            .depth(100)
            .0
            .iter()
            .chain(e.book("ETH-USDC").unwrap().depth(100).1.iter())
            .map(|(_, q)| q.0)
            .sum();
        assert_eq!(submitted, resting + 2 * traded);
        assert_eq!(traded, e.stats.volume);
    }
}
