//! Inventory domain: items, stock levels and a tiny text format.

pub mod parser;

use std::collections::BTreeMap;
use std::fmt;

/// A stock-keeping unit, e.g. `SKU-0042`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sku(pub String);

impl fmt::Display for Sku {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub sku: Sku,
    pub name: String,
    pub price_cents: u64,
    pub quantity: u32,
}

impl Item {
    pub fn new(sku: &str, name: &str, price_cents: u64, quantity: u32) -> Self {
        Self { sku: Sku(sku.to_string()), name: name.to_string(), price_cents, quantity }
    }

    pub fn total_value(&self) -> u64 {
        self.price_cents * u64::from(self.quantity)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum InventoryError {
    UnknownSku(Sku),
    InsufficientStock { sku: Sku, requested: u32, available: u32 },
}

impl fmt::Display for InventoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InventoryError::UnknownSku(sku) => write!(f, "unknown SKU {sku}"),
            InventoryError::InsufficientStock { sku, requested, available } => {
                write!(f, "cannot take {requested} of {sku}: only {available} left")
            }
        }
    }
}

impl std::error::Error for InventoryError {}

#[derive(Default, Debug)]
pub struct Inventory {
    items: BTreeMap<Sku, Item>,
}

impl Inventory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, item: Item) {
        match self.items.get_mut(&item.sku) {
            Some(existing) => existing.quantity += item.quantity,
            None => {
                self.items.insert(item.sku.clone(), item);
            }
        }
    }

    pub fn take(&mut self, sku: &Sku, quantity: u32) -> Result<u32, InventoryError> {
        let item = self.items.get_mut(sku).ok_or_else(|| InventoryError::UnknownSku(sku.clone()))?;
        if item.quantity < quantity {
            return Err(InventoryError::InsufficientStock {
                sku: sku.clone(),
                requested: quantity,
                available: item.quantity,
            });
        }
        item.quantity -= quantity;
        Ok(item.quantity)
    }

    pub fn get(&self, sku: &Sku) -> Option<&Item> {
        self.items.get(sku)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Total value of all stock in cents.
    pub fn total_value(&self) -> u64 {
        self.items.values().map(Item::total_value).sum()
    }

    // TODO: support reservations that expire after a timeout
    pub fn low_stock(&self, threshold: u32) -> Vec<&Item> {
        self.items.values().filter(|i| i.quantity <= threshold).collect()
    }
}

/// Formats cents as `$12.34`.
pub fn format_price(cents: u64) -> String {
    format!("${}.{:02}", cents / 100, cents % 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Inventory {
        let mut inv = Inventory::new();
        inv.add(Item::new("SKU-1", "Keyboard", 4999, 3));
        inv.add(Item::new("SKU-2", "Mouse", 1999, 10));
        inv
    }

    #[test]
    fn adds_and_merges_quantities() {
        let mut inv = sample();
        inv.add(Item::new("SKU-1", "Keyboard", 4999, 2));
        assert_eq!(inv.get(&Sku("SKU-1".into())).unwrap().quantity, 5);
        assert_eq!(inv.len(), 2);
    }

    #[test]
    fn take_reduces_stock() {
        let mut inv = sample();
        assert_eq!(inv.take(&Sku("SKU-2".into()), 4), Ok(6));
    }

    #[test]
    fn take_rejects_overdraw() {
        let mut inv = sample();
        let err = inv.take(&Sku("SKU-1".into()), 7).unwrap_err();
        assert_eq!(err.to_string(), "cannot take 7 of SKU-1: only 3 left");
    }

    #[test]
    fn total_value_sums_items() {
        assert_eq!(sample().total_value(), 3 * 4999 + 10 * 1999);
    }

    #[test]
    fn formats_prices() {
        assert_eq!(format_price(4999), "$49.99");
        assert_eq!(format_price(5), "$0.05");
    }

    #[test]
    #[cfg_attr(not(feature = "failing-tests"), ignore = "enable with --features failing-tests")]
    fn intentionally_failing() {
        // FIXME: this assertion is wrong on purpose (QA: failing test in the test tree)
        assert_eq!(format_price(100), "$1.0");
    }
}
