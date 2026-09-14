//! A single symbol's limit order book, with price-time priority.
//!
//! Price priority comes from `BTreeMap`, which keeps levels sorted: the best
//! bid is the last key, the best ask the first. Time priority comes from a
//! `VecDeque` per level — orders arrive at the back and fill from the front,
//! and because feed ids are strictly increasing, arrival order *is* id order.
//!
//! The book stores orders and reports crossing; it does not decide what a
//! cross means. Matching lives in the matcher.

use crate::engine::types::{Order, Price, Qty};
use crate::feed::{OrderId, Side};
use std::collections::{BTreeMap, HashMap, VecDeque};

/// Aggregated price levels for one side, best first.
pub type DepthLevels = Vec<(Price, Qty)>;

#[derive(Debug, Default)]
pub struct OrderBook {
    bids: BTreeMap<Price, VecDeque<Order>>,
    asks: BTreeMap<Price, VecDeque<Order>>,
    /// Where each resting order lives, so a cancel is a lookup rather than a
    /// scan of every price level.
    index: HashMap<OrderId, (Side, Price)>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rests an order at the back of its price level.
    pub fn insert(&mut self, order: Order) {
        let (side, price) = (order.side, order.price);
        self.levels_mut(side)
            .entry(price)
            .or_default()
            .push_back(order);
        self.index.insert(order.id, (side, price));
    }

    /// Removes a resting order and returns it, so the caller can account for
    /// the quantity that left the book unfilled. `None` means it is not in the
    /// book — already filled, already cancelled, or never seen. That is a
    /// normal outcome on this feed, not an error.
    pub fn cancel(&mut self, id: OrderId) -> Option<Order> {
        let (side, price) = self.index.remove(&id)?;
        let levels = self.levels_mut(side);
        let level = levels.get_mut(&price)?;
        let pos = level.iter().position(|o| o.id == id)?;
        let removed = level.remove(pos);
        if level.is_empty() {
            levels.remove(&price);
        }
        removed
    }

    /// The best resting order on `side`: best price, then earliest arrival.
    pub fn best_order(&self, side: Side) -> Option<&Order> {
        self.level_at_best(side).and_then(|level| level.front())
    }

    /// Takes `qty` off the best resting order on `side`, removing it if that
    /// exhausts it. Returns the order as it stood *before* the reduction, so
    /// the caller can record the maker's price and account.
    ///
    /// Panics if `qty` exceeds the resting quantity; the matcher always passes
    /// `min(taker, maker)`, so that would be a bug in the caller.
    pub fn reduce_best(&mut self, side: Side, qty: Qty) -> Option<Order> {
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        let price = match side {
            Side::Buy => *levels.keys().next_back()?,
            Side::Sell => *levels.keys().next()?,
        };
        let level = levels.get_mut(&price)?;
        let maker = *level.front()?;
        assert!(
            qty.0 <= maker.qty.0,
            "reduce_best asked for {} against a resting {}",
            qty,
            maker.qty
        );

        let remaining = Qty(maker.qty.0 - qty.0);
        if remaining.is_zero() {
            level.pop_front();
            self.index.remove(&maker.id);
            if level.is_empty() {
                levels.remove(&price);
            }
        } else {
            level.front_mut()?.qty = remaining;
        }
        Some(maker)
    }

    pub fn best_bid(&self) -> Option<Price> {
        self.bids.keys().next_back().copied()
    }

    pub fn best_ask(&self) -> Option<Price> {
        self.asks.keys().next().copied()
    }

    /// True when the best bid is at or above the best ask. After the matcher
    /// has processed an order this must always be false — it is the core
    /// invariant the tests assert.
    pub fn is_crossed(&self) -> bool {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => bid >= ask,
            _ => false,
        }
    }

    /// How many orders are resting.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Total quantity resting on both sides.
    pub fn total_qty(&self) -> Qty {
        let side_total = |levels: &BTreeMap<Price, VecDeque<Order>>| -> i64 {
            levels
                .values()
                .flat_map(|l| l.iter())
                .map(|o| o.qty.0)
                .sum()
        };
        Qty(side_total(&self.bids) + side_total(&self.asks))
    }

    /// Top `n` levels per side as (price, aggregate quantity), best first.
    pub fn depth(&self, n: usize) -> (DepthLevels, DepthLevels) {
        let sum = |level: &VecDeque<Order>| Qty(level.iter().map(|o| o.qty.0).sum());
        let bids = self
            .bids
            .iter()
            .rev()
            .take(n)
            .map(|(p, l)| (*p, sum(l)))
            .collect();
        let asks = self
            .asks
            .iter()
            .take(n)
            .map(|(p, l)| (*p, sum(l)))
            .collect();
        (bids, asks)
    }

    fn levels_mut(&mut self, side: Side) -> &mut BTreeMap<Price, VecDeque<Order>> {
        match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        }
    }

    fn level_at_best(&self, side: Side) -> Option<&VecDeque<Order>> {
        match side {
            Side::Buy => self.bids.iter().next_back().map(|(_, l)| l),
            Side::Sell => self.asks.iter().next().map(|(_, l)| l),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(id: OrderId, side: Side, price: i64, qty: i64) -> Order {
        Order {
            id,
            account: 1,
            side,
            price: Price(price),
            qty: Qty(qty),
        }
    }

    #[test]
    fn an_inserted_order_rests_and_sets_the_best_price() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Buy, 10000, 50));
        assert_eq!(book.best_bid(), Some(Price(10000)));
        assert_eq!(book.best_ask(), None);
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn best_bid_is_the_highest_and_best_ask_the_lowest() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Buy, 9900, 10));
        book.insert(order(2, Side::Buy, 10100, 10));
        book.insert(order(3, Side::Buy, 10000, 10));
        book.insert(order(4, Side::Sell, 10500, 10));
        book.insert(order(5, Side::Sell, 10300, 10));
        book.insert(order(6, Side::Sell, 10400, 10));
        assert_eq!(book.best_bid(), Some(Price(10100)));
        assert_eq!(book.best_ask(), Some(Price(10300)));
    }

    #[test]
    fn orders_at_the_same_price_keep_arrival_order() {
        let mut book = OrderBook::new();
        book.insert(order(7, Side::Buy, 10000, 10));
        book.insert(order(8, Side::Buy, 10000, 10));
        book.insert(order(9, Side::Buy, 10000, 10));
        // Lowest id arrived first, so it is at the front.
        assert_eq!(book.best_order(Side::Buy).unwrap().id, 7);
        book.reduce_best(Side::Buy, Qty(10));
        assert_eq!(book.best_order(Side::Buy).unwrap().id, 8);
    }

    #[test]
    fn a_better_price_outranks_an_earlier_arrival() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Buy, 10000, 10));
        book.insert(order(2, Side::Buy, 10100, 10));
        assert_eq!(book.best_order(Side::Buy).unwrap().id, 2);
    }

    #[test]
    fn cancel_removes_a_resting_order() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Buy, 10000, 50));
        assert!(book.cancel(1).is_some());
        assert!(book.is_empty());
        assert_eq!(book.best_bid(), None);
    }

    #[test]
    fn cancel_for_an_unknown_order_is_a_no_op() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Buy, 10000, 50));
        assert!(book.cancel(9999).is_none());
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn cancel_twice_reports_false_the_second_time() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Buy, 10000, 50));
        assert!(book.cancel(1).is_some());
        assert!(book.cancel(1).is_none());
    }

    #[test]
    fn cancel_returns_the_unfilled_remainder() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Sell, 10000, 50));
        book.reduce_best(Side::Sell, Qty(31));
        // 1.9 was still resting when the cancel arrived; that quantity left
        // the book without ever trading.
        let removed = book.cancel(1).expect("order should be resting");
        assert_eq!(removed.qty, Qty(19));
    }

    #[test]
    fn cancelling_the_last_order_at_a_price_drops_the_level() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Buy, 10000, 10));
        book.insert(order(2, Side::Buy, 9900, 10));
        book.cancel(1);
        assert_eq!(book.best_bid(), Some(Price(9900)));
    }

    #[test]
    fn reduce_best_takes_a_partial_fill_and_leaves_the_remainder() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Sell, 10000, 50));
        let maker = book.reduce_best(Side::Sell, Qty(31)).unwrap();
        // The snapshot reports the pre-fill state.
        assert_eq!(maker.qty, Qty(50));
        assert_eq!(book.best_order(Side::Sell).unwrap().qty, Qty(19));
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn reduce_best_removes_an_exhausted_order_and_deindexes_it() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Sell, 10000, 50));
        book.reduce_best(Side::Sell, Qty(50));
        assert!(book.is_empty());
        assert_eq!(book.best_ask(), None);
        // Deindexed, so a later cancel takes the no-op path.
        assert!(book.cancel(1).is_none());
    }

    #[test]
    fn is_crossed_detects_a_crossed_book() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Buy, 10100, 10));
        assert!(!book.is_crossed());
        book.insert(order(2, Side::Sell, 10000, 10));
        // The book stores this happily; resolving it is the matcher's job.
        assert!(book.is_crossed());
    }

    #[test]
    fn depth_aggregates_quantity_per_level_best_first() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Buy, 10000, 10));
        book.insert(order(2, Side::Buy, 10000, 25));
        book.insert(order(3, Side::Buy, 9900, 5));
        book.insert(order(4, Side::Sell, 10200, 7));
        book.insert(order(5, Side::Sell, 10300, 8));
        let (bids, asks) = book.depth(2);
        assert_eq!(bids, vec![(Price(10000), Qty(35)), (Price(9900), Qty(5))]);
        assert_eq!(asks, vec![(Price(10200), Qty(7)), (Price(10300), Qty(8))]);
    }
}
