#![forbid(unsafe_code)]
//! # me-book — Order Book and Matching Algorithm
//!
//! This crate implements the order book data structure and the price-time priority
//! matching algorithm used by the matching engine.
//!
//! ## Architecture
//!
//! The [`book::OrderBook`] uses a combination of data structures for optimal performance:
//!
//! - **`BTreeMap<FixedPrice, PriceLevel>`** — Price-level indexing (O(log n) insert/lookup)
//! - **`Slab<OrderNode>`** — Order node allocation with O(1) alloc/dealloc via free list
//! - **Intrusive linked list** — FIFO ordering within each price level (O(1) insert/remove)
//! - **`FxHashMap<OrderId, OrderLocation>`** — O(1) order lookup by ID for cancel/amend
//!
//! ## Matching Algorithm
//!
//! Price-time priority (FIFO within each level):
//! - **Buy orders** match against asks from lowest price upward
//! - **Sell orders** match against bids from highest price downward
//! - **FOK** checks available liquidity before executing
//! - **IOC** cancels unfilled remainder
//! - **Market orders** sweep the opposite side
//!
//! ## Stop Orders
//!
//! [`stop_book::StopBook`] holds stop orders separately. When a trade occurs at or
//! through the stop price, the stop order is triggered and converted to a regular
//! order (Limit or Market). Cascading triggers are bounded to 100 iterations.

pub mod book;
pub mod matching;
pub mod pool;
pub mod price_level;
pub mod stop_book;
