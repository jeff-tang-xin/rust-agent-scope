import { Cart, type Product } from "./cart.ts";
import { cents, formatMoney } from "./money.ts";

const catalog: Product[] = [
  { id: "kb", name: "Mechanical keyboard", price: cents(8999) },
  { id: "ms", name: "Wireless mouse", price: cents(2999) },
  { id: "hub", name: "USB-C hub", price: cents(3999) },
];

function main(): void {
  const cart = new Cart();
  cart.add(catalog[0]!).add(catalog[1]!, 2);
  cart.applyDiscount({ kind: "percent", percent: 10 });

  // Set a breakpoint here (QA: debugger) and inspect `cart`.
  const total = cart.total();
  for (const line of cart.items) {
    console.log(`${line.quantity} x ${line.product.name}`);
  }
  console.log(`Total: ${formatMoney(total)}`);
}

// FIXME: unused helper kept on purpose (QA: "unused" inspection)
function unusedHelper(value: number) {
  return value * 2;
}

main();
