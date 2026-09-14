//! A matching engine that consumes the order feed.
//!
//! The feed publishes limit orders and cancels but never matches them. This
//! module is the missing half: it folds the message stream into per-symbol
//! order books and infers the trades the exchange would have printed.
//!
//! The fold is pure — no network, no clock — so the whole engine is testable
//! against a canned sequence of events with no server running.

pub mod book;
pub mod cursor;
pub mod market;
pub mod matcher;
pub mod types;

pub use book::OrderBook;
pub use cursor::Cursor;
pub use market::{MarketState, SymbolState};
pub use matcher::{Engine, Stats};
pub use types::{ConvertError, Event, Order, Price, Qty, Symbol, Trade, resolve_symbol};
