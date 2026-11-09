use inventory::{Inventory, Sku, format_price, parser};

const SAMPLE: &str = "\
# sku;name;price_cents;quantity
SKU-0001;Mechanical keyboard;8999;4
SKU-0002;Wireless mouse;2999;12
SKU-0003;USB-C hub;3999;2
";

fn main() {
    let items = match parser::parse(SAMPLE) {
        Ok(items) => items,
        Err(e) => {
            eprintln!("line {}: {}", e.line, e.message);
            std::process::exit(1);
        }
    };
    let mut inventory = Inventory::new();
    for item in items {
        inventory.add(item);
    }

    // Set a breakpoint here (QA: debugger) and inspect `inventory`.
    let remaining = inventory.take(&Sku("SKU-0002".into()), 3);
    match remaining {
        Ok(left) => println!("Sold 3 mice, {left} left"),
        Err(e) => eprintln!("error: {e}"),
    }

    for item in inventory.low_stock(3) {
        println!("Low stock: {} ({}) x{}", item.name, item.sku, item.quantity);
    }
    println!("Inventory value: {}", format_price(inventory.total_value()));
}
