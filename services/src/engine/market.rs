//! Derived market state: what the tape says once the matching is done.
//!
//! The feed publishes no trades, so every number here is inferred from the
//! trades the matcher produced. This module only ever consumes `Trade`s — it
//! never touches the book — which keeps "what happened" separate from "what is
//! resting".
//!
//! Net positions are tracked per (account, symbol). There are no balances: the
//! feed has no deposits, no starting funds and no settlement, so a cash balance
//! would be invented. A net position is not — it is just signed traded
//! quantity, and it is exactly derivable from the tape.

use crate::engine::types::{Price, Qty, Symbol, Trade};
use crate::feed::{AccountId, Side};
use std::collections::HashMap;

/// Per-symbol tape statistics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SymbolState {
    pub trades: u64,
    /// Traded quantity in ticks (tenths).
    pub volume: i64,
    /// Sum of price_ticks * qty_ticks, used for VWAP.
    pub notional: i64,
    pub last: Option<Price>,
    pub high: Option<Price>,
    pub low: Option<Price>,
}

impl SymbolState {
    /// Volume-weighted average price. `None` before the first trade.
    pub fn vwap(&self) -> Option<Price> {
        if self.volume == 0 {
            None
        } else {
            Some(Price(self.notional / self.volume))
        }
    }
}

#[derive(Debug, Default)]
pub struct MarketState {
    symbols: HashMap<Symbol, SymbolState>,
    /// Signed traded quantity in ticks: positive is long, negative is short.
    positions: HashMap<(AccountId, Symbol), i64>,
}

impl MarketState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one trade into the derived state.
    pub fn record(&mut self, trade: &Trade) {
        let s = self.symbols.entry(trade.symbol).or_default();
        s.trades += 1;
        s.volume += trade.qty.0;
        s.notional += trade.price.0 * trade.qty.0;
        s.last = Some(trade.price);
        s.high = Some(s.high.map_or(trade.price, |h| h.max(trade.price)));
        s.low = Some(s.low.map_or(trade.price, |l| l.min(trade.price)));

        // `taker_side` is the aggressor's side, so it identifies who bought.
        let (buyer, seller) = match trade.taker_side {
            Side::Buy => (trade.taker_account, trade.maker_account),
            Side::Sell => (trade.maker_account, trade.taker_account),
        };
        *self.positions.entry((buyer, trade.symbol)).or_default() += trade.qty.0;
        *self.positions.entry((seller, trade.symbol)).or_default() -= trade.qty.0;
    }

    pub fn symbol(&self, symbol: Symbol) -> Option<&SymbolState> {
        self.symbols.get(symbol)
    }

    pub fn symbols(&self) -> impl Iterator<Item = (&Symbol, &SymbolState)> {
        self.symbols.iter()
    }

    /// Net position for one account in one symbol, in quantity ticks.
    pub fn position(&self, account: AccountId, symbol: Symbol) -> i64 {
        self.positions.get(&(account, symbol)).copied().unwrap_or(0)
    }

    /// Every non-flat position, largest absolute size first.
    pub fn top_positions(&self, n: usize) -> Vec<(AccountId, Symbol, Qty)> {
        let mut v: Vec<_> = self
            .positions
            .iter()
            .filter(|(_, qty)| **qty != 0)
            .map(|((account, symbol), qty)| (*account, *symbol, Qty(*qty)))
            .collect();
        v.sort_by_key(|(account, symbol, qty)| (-qty.0.abs(), *account, *symbol));
        v.truncate(n);
        v
    }

    /// Sum of every position. Must always be zero: each unit bought by someone
    /// was sold by someone else. A non-zero value means trades are being
    /// double-counted or attributed to the wrong side.
    pub fn net_position(&self) -> i64 {
        self.positions.values().sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ETH: Symbol = "ETH-USDC";
    const BTC: Symbol = "BTC-USDC";

    fn trade(symbol: Symbol, price: i64, qty: i64, maker: u32, taker: u32, side: Side) -> Trade {
        Trade {
            seq: 1,
            symbol,
            price: Price(price),
            qty: Qty(qty),
            maker_order: 1,
            maker_account: maker,
            taker_order: 2,
            taker_account: taker,
            taker_side: side,
        }
    }

    #[test]
    fn records_last_high_low_and_volume() {
        let mut m = MarketState::new();
        m.record(&trade(ETH, 10000, 50, 1, 2, Side::Buy));
        m.record(&trade(ETH, 10200, 30, 1, 2, Side::Buy));
        m.record(&trade(ETH, 9900, 20, 1, 2, Side::Sell));

        let s = m.symbol(ETH).unwrap();
        assert_eq!(s.trades, 3);
        assert_eq!(s.volume, 100);
        assert_eq!(s.last, Some(Price(9900)));
        assert_eq!(s.high, Some(Price(10200)));
        assert_eq!(s.low, Some(Price(9900)));
    }

    #[test]
    fn vwap_is_weighted_by_quantity_not_trade_count() {
        let mut m = MarketState::new();
        // 9.0 at 100.00 and 1.0 at 200.00 -> 110.00, not the 150.00 a plain
        // average of the two prices would give.
        m.record(&trade(ETH, 10000, 90, 1, 2, Side::Buy));
        m.record(&trade(ETH, 20000, 10, 1, 2, Side::Buy));
        assert_eq!(m.symbol(ETH).unwrap().vwap(), Some(Price(11000)));
    }

    #[test]
    fn vwap_is_none_before_any_trade() {
        let m = MarketState::new();
        assert!(m.symbol(ETH).is_none());
        assert_eq!(SymbolState::default().vwap(), None);
    }

    #[test]
    fn an_aggressive_buy_makes_the_taker_long_and_the_maker_short() {
        let mut m = MarketState::new();
        m.record(&trade(ETH, 10000, 50, 7, 9, Side::Buy));
        assert_eq!(m.position(9, ETH), 50); // taker bought
        assert_eq!(m.position(7, ETH), -50); // maker sold
    }

    #[test]
    fn an_aggressive_sell_reverses_the_attribution() {
        let mut m = MarketState::new();
        m.record(&trade(ETH, 10000, 50, 7, 9, Side::Sell));
        assert_eq!(m.position(9, ETH), -50); // taker sold
        assert_eq!(m.position(7, ETH), 50); // maker bought
    }

    #[test]
    fn positions_are_tracked_per_symbol() {
        let mut m = MarketState::new();
        m.record(&trade(ETH, 10000, 50, 1, 2, Side::Buy));
        m.record(&trade(BTC, 99000, 30, 1, 2, Side::Buy));
        assert_eq!(m.position(2, ETH), 50);
        assert_eq!(m.position(2, BTC), 30);
        assert_eq!(m.position(2, "NEX-USDC"), 0);
    }

    #[test]
    fn a_self_trade_leaves_the_account_flat() {
        let mut m = MarketState::new();
        m.record(&trade(ETH, 10000, 50, 5, 5, Side::Buy));
        assert_eq!(m.position(5, ETH), 0);
    }

    #[test]
    fn every_position_nets_to_zero_across_accounts() {
        let mut m = MarketState::new();
        m.record(&trade(ETH, 10000, 50, 1, 2, Side::Buy));
        m.record(&trade(ETH, 10100, 30, 3, 1, Side::Sell));
        m.record(&trade(BTC, 99000, 70, 2, 3, Side::Buy));
        m.record(&trade(ETH, 9900, 10, 5, 5, Side::Buy));
        // Each unit bought was sold by someone else.
        assert_eq!(m.net_position(), 0);
    }

    #[test]
    fn top_positions_ranks_by_absolute_size_and_skips_flat_accounts() {
        let mut m = MarketState::new();
        m.record(&trade(ETH, 10000, 90, 1, 2, Side::Buy)); // 2 long 90, 1 short 90
        m.record(&trade(ETH, 10000, 10, 3, 4, Side::Buy)); // 4 long 10, 3 short 10
        m.record(&trade(ETH, 10000, 50, 5, 5, Side::Buy)); // 5 flat
        let top = m.top_positions(3);
        assert_eq!(top.len(), 3);
        assert_eq!(top[0].2.0.abs(), 90);
        assert!(!top.iter().any(|(account, _, _)| *account == 5));
    }
}
