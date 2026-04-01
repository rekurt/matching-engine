use me_core::order::Order;
use me_core::types::{FixedPrice, FixedQty};

use crate::pool::{Slab, SlabKey};

/// A node in the intrusive doubly-linked list within a price level.
pub struct OrderNode {
    pub order: Order,
    pub prev: Option<SlabKey>,
    pub next: Option<SlabKey>,
}

/// A single price level: a FIFO queue of orders at one price.
/// Backed by an external slab allocator for O(1) insert/remove operations.
pub struct PriceLevel {
    pub price: FixedPrice,
    pub total_qty: FixedQty,
    pub order_count: u32,
    head: Option<SlabKey>,
    tail: Option<SlabKey>,
}

impl PriceLevel {
    pub fn new(price: FixedPrice) -> Self {
        Self {
            price,
            total_qty: FixedQty::ZERO,
            order_count: 0,
            head: None,
            tail: None,
        }
    }

    /// Is this level empty?
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.order_count == 0
    }

    /// Get the head (front) slab key for iteration.
    #[inline]
    pub fn head(&self) -> Option<SlabKey> {
        self.head
    }

    /// Append an order to the tail of the FIFO queue.
    /// Returns the slab key of the inserted order node.
    pub fn push_back(&mut self, order: Order, slab: &mut Slab<OrderNode>) -> SlabKey {
        let qty = order.remaining_qty;
        let node = OrderNode {
            order,
            prev: self.tail,
            next: None,
        };
        let key = slab.insert(node);

        if let Some(tail_key) = self.tail {
            slab.get_mut(tail_key).unwrap().next = Some(key);
        } else {
            self.head = Some(key);
        }
        self.tail = Some(key);

        self.total_qty += qty;
        self.order_count += 1;
        key
    }

    /// Remove an order node by its slab key from the linked list.
    /// Returns the removed Order.
    pub fn remove(&mut self, key: SlabKey, slab: &mut Slab<OrderNode>) -> Order {
        let node = slab.remove(key);
        let remaining = node.order.remaining_qty;

        // Update linked list pointers.
        match (node.prev, node.next) {
            (Some(prev), Some(next)) => {
                slab.get_mut(prev).unwrap().next = Some(next);
                slab.get_mut(next).unwrap().prev = Some(prev);
            }
            (Some(prev), None) => {
                slab.get_mut(prev).unwrap().next = None;
                self.tail = Some(prev);
            }
            (None, Some(next)) => {
                slab.get_mut(next).unwrap().prev = None;
                self.head = Some(next);
            }
            (None, None) => {
                self.head = None;
                self.tail = None;
            }
        }

        self.total_qty -= remaining;
        self.order_count -= 1;
        node.order
    }

    /// Pop the front (oldest) order from the queue.
    pub fn pop_front(&mut self, slab: &mut Slab<OrderNode>) -> Option<Order> {
        let head_key = self.head?;
        Some(self.remove(head_key, slab))
    }

    /// Peek at the front order without removing it.
    pub fn peek_front<'a>(&self, slab: &'a Slab<OrderNode>) -> Option<&'a Order> {
        let head_key = self.head?;
        slab.get(head_key).map(|node| &node.order)
    }
}

/// Iterator over order node keys at a price level (front to back).
pub struct PriceLevelIter {
    current: Option<SlabKey>,
}

impl PriceLevelIter {
    pub fn new(level: &PriceLevel) -> Self {
        Self {
            current: level.head(),
        }
    }

    /// Advance to the next key, returning the current one.
    pub fn next_key(&mut self, slab: &Slab<OrderNode>) -> Option<SlabKey> {
        let key = self.current?;
        self.current = slab.get(key).and_then(|node| node.next);
        Some(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::order::Order;
    use me_core::order::TimeInForce;
    use me_core::types::*;

    fn make_order(id: OrderId, qty: f64) -> Order {
        Order::new_limit(
            id,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(100.0),
            FixedQty::from_f64(qty),
            id * 1000,
        )
    }

    #[test]
    fn test_push_and_pop_fifo() {
        let mut slab = Slab::with_capacity(8);
        let mut level = PriceLevel::new(FixedPrice::from_f64(100.0));

        level.push_back(make_order(1, 1.0), &mut slab);
        level.push_back(make_order(2, 2.0), &mut slab);
        level.push_back(make_order(3, 3.0), &mut slab);

        assert_eq!(level.order_count, 3);
        assert_eq!(level.total_qty, FixedQty::from_f64(6.0));

        let o1 = level.pop_front(&mut slab).unwrap();
        assert_eq!(o1.id, 1);
        let o2 = level.pop_front(&mut slab).unwrap();
        assert_eq!(o2.id, 2);
        let o3 = level.pop_front(&mut slab).unwrap();
        assert_eq!(o3.id, 3);
        assert!(level.is_empty());
    }

    #[test]
    fn test_remove_middle() {
        let mut slab = Slab::with_capacity(8);
        let mut level = PriceLevel::new(FixedPrice::from_f64(100.0));

        level.push_back(make_order(1, 1.0), &mut slab);
        let k2 = level.push_back(make_order(2, 2.0), &mut slab);
        level.push_back(make_order(3, 3.0), &mut slab);

        let removed = level.remove(k2, &mut slab);
        assert_eq!(removed.id, 2);
        assert_eq!(level.order_count, 2);
        assert_eq!(level.total_qty, FixedQty::from_f64(4.0));

        // FIFO order preserved: 1, 3
        let o1 = level.pop_front(&mut slab).unwrap();
        assert_eq!(o1.id, 1);
        let o3 = level.pop_front(&mut slab).unwrap();
        assert_eq!(o3.id, 3);
    }

    #[test]
    fn test_remove_head() {
        let mut slab = Slab::with_capacity(8);
        let mut level = PriceLevel::new(FixedPrice::from_f64(100.0));

        let k1 = level.push_back(make_order(1, 1.0), &mut slab);
        level.push_back(make_order(2, 2.0), &mut slab);

        level.remove(k1, &mut slab);
        let o2 = level.pop_front(&mut slab).unwrap();
        assert_eq!(o2.id, 2);
        assert!(level.is_empty());
    }

    #[test]
    fn test_remove_tail() {
        let mut slab = Slab::with_capacity(8);
        let mut level = PriceLevel::new(FixedPrice::from_f64(100.0));

        level.push_back(make_order(1, 1.0), &mut slab);
        let k2 = level.push_back(make_order(2, 2.0), &mut slab);

        level.remove(k2, &mut slab);
        let o1 = level.pop_front(&mut slab).unwrap();
        assert_eq!(o1.id, 1);
        assert!(level.is_empty());
    }

    #[test]
    fn test_peek_front() {
        let mut slab = Slab::with_capacity(4);
        let mut level = PriceLevel::new(FixedPrice::from_f64(50.0));

        assert!(level.peek_front(&slab).is_none());
        level.push_back(make_order(1, 1.0), &mut slab);
        assert_eq!(level.peek_front(&slab).unwrap().id, 1);
    }

    #[test]
    fn test_price_level_iter() {
        let mut slab = Slab::with_capacity(8);
        let mut level = PriceLevel::new(FixedPrice::from_f64(100.0));

        level.push_back(make_order(1, 1.0), &mut slab);
        level.push_back(make_order(2, 2.0), &mut slab);
        level.push_back(make_order(3, 3.0), &mut slab);

        let mut iter = PriceLevelIter::new(&level);
        let mut ids = vec![];
        while let Some(key) = iter.next_key(&slab) {
            ids.push(slab.get(key).unwrap().order.id);
        }
        assert_eq!(ids, vec![1, 2, 3]);
    }
}
