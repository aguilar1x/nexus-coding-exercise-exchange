//! Normalized domain types for the matching engine.
//!
//! The feed publishes prices and quantities as `f64`. Everything inside the
//! engine works in integer ticks instead: partial fills are then exact, and a
//! fully filled order compares equal to zero instead of leaving float dust
//! behind that would keep it wedged in the book forever.

use crate::feed::{AccountId, OrderId, OrderMessage, SYMBOLS, Side};
use std::fmt;

/// Symbols are interned against `feed::SYMBOLS`, so a `Symbol` is both a
/// validated trading pair and a cheap `Copy` key. Resolution doubles as
/// validation and costs no allocation per message.
pub type Symbol = &'static str;

/// Prices in cents. The feed rounds to 2 decimals at the source
/// (`feed::round2`), so this conversion is lossless.
pub const PRICE_SCALE: f64 = 100.0;

/// Quantities in tenths. The feed rounds to 1 decimal at the source
/// (`feed::round1`), so this conversion is lossless.
pub const QTY_SCALE: f64 = 10.0;

/// A price in whole cents: `100.25` is `Price(10025)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Price(pub i64);

/// A quantity in tenths of a unit: `5.0` is `Qty(50)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Qty(pub i64);

impl Price {
    pub fn from_f64(x: f64) -> Result<Self, ConvertError> {
        if !x.is_finite() || x <= 0.0 {
            return Err(ConvertError::BadPrice(x));
        }
        Ok(Price((x * PRICE_SCALE).round() as i64))
    }

    pub fn to_f64(self) -> f64 {
        self.0 as f64 / PRICE_SCALE
    }
}

impl Qty {
    pub fn from_f64(x: f64) -> Result<Self, ConvertError> {
        if !x.is_finite() || x <= 0.0 {
            return Err(ConvertError::BadQuantity(x));
        }
        Ok(Qty((x * QTY_SCALE).round() as i64))
    }

    pub fn to_f64(self) -> f64 {
        self.0 as f64 / QTY_SCALE
    }

    pub fn is_zero(self) -> bool {
        self.0 == 0
    }
}

// Both impls route through `f.pad` rather than writing directly, so width and
// alignment specifiers (`{:>9}`) are honoured and the printed columns line up.
// The sign is applied to the formatted string rather than taken from the
// integer division: `-2 / 10` is `0`, so a naive impl prints "0.2" for a short
// position of -0.2 and silently drops the sign.
fn signed(value: i64, scale: i64, decimals: usize) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let v = value.abs();
    format!(
        "{}{}.{:0width$}",
        sign,
        v / scale,
        v % scale,
        width = decimals
    )
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&signed(self.0, 100, 2))
    }
}

impl fmt::Display for Qty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&signed(self.0, 10, 1))
    }
}

/// A limit order as the engine tracks it.
///
/// `id` is the feed message id that introduced the order. Because feed ids are
/// strictly increasing, `id` is also the arrival timestamp for time priority —
/// there is no separate sequence field, and the wire `timestamp` is never used
/// for ordering (it is wall-clock and non-monotonic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Order {
    pub id: OrderId,
    pub account: AccountId,
    pub side: Side,
    pub price: Price,
    /// Remaining quantity. Decremented on each fill; the order leaves the book
    /// when this reaches exactly zero.
    pub qty: Qty,
}

/// A feed message normalized into engine terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    New {
        symbol: Symbol,
        order: Order,
    },
    /// Only `target_id` and `account` are consumed from the wire message.
    /// `seq` is the cancel's own feed id, kept purely as the cursor / ordering
    /// key. The wire `timestamp` is dropped.
    ///
    /// There is deliberately no `symbol` here: `OrderMessage::Cancel` does not
    /// carry one, and which book holds `target_id` depends on accumulated
    /// engine state that this stateless conversion cannot see. The symbol is
    /// resolved in the matcher instead.
    Cancel {
        seq: OrderId,
        account: AccountId,
        target_id: OrderId,
    },
}

impl Event {
    /// The feed sequence id of the message this event came from.
    pub fn seq(&self) -> OrderId {
        match self {
            Event::New { order, .. } => order.id,
            Event::Cancel { seq, .. } => *seq,
        }
    }
}

/// A trade the engine inferred. The feed never publishes these — two orders
/// crossing is our conclusion, not the exchange's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trade {
    /// The feed message id that caused this trade.
    pub seq: OrderId,
    pub symbol: Symbol,
    /// The resting (maker) order's price. The aggressor never sets the price.
    pub price: Price,
    pub qty: Qty,
    pub maker_order: OrderId,
    pub maker_account: AccountId,
    pub taker_order: OrderId,
    pub taker_account: AccountId,
    pub taker_side: Side,
}

/// Why a feed message could not be normalized. The runner logs and skips these
/// rather than aborting the poll loop.
#[derive(Debug, Clone, PartialEq)]
pub enum ConvertError {
    UnknownSymbol(String),
    BadPrice(f64),
    BadQuantity(f64),
}

impl fmt::Display for ConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConvertError::UnknownSymbol(s) => write!(f, "unknown symbol '{}'", s),
            ConvertError::BadPrice(p) => write!(f, "invalid price {}", p),
            ConvertError::BadQuantity(q) => write!(f, "invalid quantity {}", q),
        }
    }
}

impl std::error::Error for ConvertError {}

/// Interns a symbol against the feed's own `SYMBOLS` table.
pub fn resolve_symbol(s: &str) -> Option<Symbol> {
    SYMBOLS.iter().map(|(sym, _)| *sym).find(|sym| *sym == s)
}

impl TryFrom<OrderMessage> for Event {
    type Error = ConvertError;

    fn try_from(msg: OrderMessage) -> Result<Self, Self::Error> {
        match msg {
            // `timestamp` is intentionally discarded: it is wall-clock, it
            // repeats across messages, and `id` is the only ordering key.
            OrderMessage::New {
                id,
                account,
                symbol,
                side,
                price,
                quantity,
                ..
            } => {
                let symbol = resolve_symbol(&symbol).ok_or(ConvertError::UnknownSymbol(symbol))?;
                Ok(Event::New {
                    symbol,
                    order: Order {
                        id,
                        account,
                        side,
                        price: Price::from_f64(price)?,
                        qty: Qty::from_f64(quantity)?,
                    },
                })
            }
            OrderMessage::Cancel {
                id,
                account,
                target_id,
                ..
            } => Ok(Event::Cancel {
                seq: id,
                account,
                target_id,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_msg(id: OrderId, symbol: &str, side: Side, price: f64, quantity: f64) -> OrderMessage {
        OrderMessage::New {
            id,
            timestamp: 1_753_000_000_000,
            account: 3,
            symbol: symbol.to_string(),
            side,
            price,
            quantity,
        }
    }

    #[test]
    fn price_conversion_is_lossless_for_two_decimals() {
        assert_eq!(Price::from_f64(100.25).unwrap(), Price(10025));
        assert_eq!(Price::from_f64(9.96).unwrap(), Price(996));
        assert_eq!(Price::from_f64(1000.0).unwrap(), Price(100000));
        assert_eq!(Price(10025).to_f64(), 100.25);
    }

    #[test]
    fn quantity_conversion_is_lossless_for_one_decimal() {
        assert_eq!(Qty::from_f64(5.0).unwrap(), Qty(50));
        assert_eq!(Qty::from_f64(8.1).unwrap(), Qty(81));
        assert_eq!(Qty(81).to_f64(), 8.1);
    }

    #[test]
    fn integer_ticks_make_partial_fills_exact() {
        // The whole reason for integer ticks: in f64 this subtraction yields
        // 1.9000000000000004, so the order never compares equal to zero and
        // never leaves the book.
        let remaining = Qty(50).0 - Qty(31).0;
        assert_eq!(Qty(remaining), Qty(19));
        assert_eq!(Qty(remaining).to_f64(), 1.9);
        assert!(Qty(Qty(50).0 - Qty(50).0).is_zero());
    }

    #[test]
    fn rejects_non_positive_and_non_finite_values() {
        assert_eq!(Price::from_f64(0.0), Err(ConvertError::BadPrice(0.0)));
        assert_eq!(Price::from_f64(-1.0), Err(ConvertError::BadPrice(-1.0)));
        assert!(matches!(
            Price::from_f64(f64::NAN),
            Err(ConvertError::BadPrice(_))
        ));
        assert_eq!(Qty::from_f64(0.0), Err(ConvertError::BadQuantity(0.0)));
    }

    #[test]
    fn resolves_known_symbols_and_rejects_others() {
        assert_eq!(resolve_symbol("ETH-USDC"), Some("ETH-USDC"));
        assert_eq!(resolve_symbol("BTC-USDC"), Some("BTC-USDC"));
        assert_eq!(resolve_symbol("NEX-USDC"), Some("NEX-USDC"));
        assert_eq!(resolve_symbol("DOGE-USDC"), None);
        // Case-sensitive, matching the feed's own table.
        assert_eq!(resolve_symbol("eth-usdc"), None);
    }

    #[test]
    fn converts_a_new_message() {
        let ev = Event::try_from(new_msg(42, "ETH-USDC", Side::Buy, 100.25, 5.0)).unwrap();
        assert_eq!(
            ev,
            Event::New {
                symbol: "ETH-USDC",
                order: Order {
                    id: 42,
                    account: 3,
                    side: Side::Buy,
                    price: Price(10025),
                    qty: Qty(50),
                },
            }
        );
        assert_eq!(ev.seq(), 42);
    }

    #[test]
    fn converts_a_cancel_keeping_only_target_and_account() {
        let msg = OrderMessage::Cancel {
            id: 57,
            timestamp: 1_753_000_004_000,
            account: 3,
            target_id: 42,
        };
        let ev = Event::try_from(msg).unwrap();
        assert_eq!(
            ev,
            Event::Cancel {
                seq: 57,
                account: 3,
                target_id: 42,
            }
        );
        assert_eq!(ev.seq(), 57);
    }

    #[test]
    fn rejects_a_new_message_with_an_unknown_symbol() {
        let err = Event::try_from(new_msg(1, "DOGE-USDC", Side::Buy, 1.0, 1.0)).unwrap_err();
        assert_eq!(err, ConvertError::UnknownSymbol("DOGE-USDC".to_string()));
    }

    #[test]
    fn display_formats_ticks_back_to_human_decimals() {
        assert_eq!(Price(10025).to_string(), "100.25");
        assert_eq!(Price(996).to_string(), "9.96");
        assert_eq!(Price(100000).to_string(), "1000.00");
        assert_eq!(Qty(50).to_string(), "5.0");
        assert_eq!(Qty(81).to_string(), "8.1");
    }

    #[test]
    fn display_keeps_the_sign_on_small_negative_values() {
        // Net positions can be short, and -2 / 10 == 0 in integer division, so
        // a naive impl would print "0.2" here and lose the direction entirely.
        assert_eq!(Qty(-2).to_string(), "-0.2");
        assert_eq!(Qty(-452).to_string(), "-45.2");
        assert_eq!(Price(-5).to_string(), "-0.05");
        assert_eq!(Qty(0).to_string(), "0.0");
    }

    #[test]
    fn display_honours_width_and_alignment() {
        // Custom Display impls must use `f.pad` or width specifiers are
        // silently dropped, which wrecks the printed columns.
        assert_eq!(format!("{:>9}", Price(10025)), "   100.25");
        assert_eq!(format!("{:>6}", Qty(50)), "   5.0");
        assert_eq!(format!("{:<8}|", Price(996)), "9.96    |");
    }
}
