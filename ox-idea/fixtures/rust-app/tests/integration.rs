use inventory::{Inventory, Item, Sku, parser};

#[test]
fn round_trip_from_text() {
    let items = parser::parse("A;Apple;50;10\nB;Banana;25;4").unwrap();
    let mut inv = Inventory::new();
    items.into_iter().for_each(|i| inv.add(i));
    assert_eq!(inv.total_value(), 50 * 10 + 25 * 4);
}

#[test]
fn unknown_sku_is_an_error() {
    let mut inv = Inventory::new();
    inv.add(Item::new("A", "Apple", 50, 1));
    assert!(inv.take(&Sku("Z".into()), 1).is_err());
}
