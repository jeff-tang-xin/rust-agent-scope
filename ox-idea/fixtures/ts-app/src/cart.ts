import { addMoney, cents, type Cents } from "./money.ts";

export interface Product {
  id: string;
  name: string;
  price: Cents;
}

export interface CartLine {
  product: Product;
  quantity: number;
}

export type Discount = { kind: "percent"; percent: number } | { kind: "fixed"; amount: Cents };

export class Cart {
  private readonly lines = new Map<string, CartLine>();
  private discount: Discount | null = null;

  add(product: Product, quantity = 1): this {
    if (quantity <= 0) throw new RangeError("quantity must be positive");
    const line = this.lines.get(product.id);
    if (line) line.quantity += quantity;
    else this.lines.set(product.id, { product, quantity });
    return this;
  }

  remove(productId: string): boolean {
    return this.lines.delete(productId);
  }

  applyDiscount(discount: Discount): void {
    // TODO: validate that percent is between 0 and 100
    this.discount = discount;
  }

  get items(): CartLine[] {
    return [...this.lines.values()];
  }

  get count(): number {
    return this.items.reduce((n, l) => n + l.quantity, 0);
  }

  subtotal(): Cents {
    return this.items.reduce((sum, l) => addMoney(sum, cents(l.product.price * l.quantity)), cents(0));
  }

  total(): Cents {
    const subtotal = this.subtotal();
    const d = this.discount;
    if (!d) return subtotal;
    if (d.kind === "fixed") return cents(Math.max(0, subtotal - d.amount));
    return cents(Math.round(subtotal * (1 - d.percent / 100)));
  }
}
