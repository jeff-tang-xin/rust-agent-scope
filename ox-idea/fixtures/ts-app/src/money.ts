/** Integer cents to avoid floating point surprises. */
export type Cents = number & { readonly __brand: "cents" };

export function cents(value: number): Cents {
  if (!Number.isInteger(value)) throw new RangeError(`cents must be an integer, got ${value}`);
  return value as Cents;
}

export function formatMoney(value: Cents, currency = "USD"): string {
  return new Intl.NumberFormat("en-US", { style: "currency", currency }).format(value / 100);
}

export function addMoney(a: Cents, b: Cents): Cents {
  return (a + b) as Cents;
}
