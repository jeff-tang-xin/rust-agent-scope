import { describe, expect, it } from "vitest";
import { Cart } from "../src/cart.ts";
import { cents, formatMoney } from "../src/money.ts";

const keyboard = { id: "kb", name: "Keyboard", price: cents(5000) };
const mouse = { id: "ms", name: "Mouse", price: cents(2000) };

describe("Cart", () => {
  it("adds and merges lines", () => {
    const cart = new Cart().add(keyboard).add(keyboard, 2).add(mouse);
    expect(cart.items).toHaveLength(2);
    expect(cart.count).toBe(4);
  });

  it("computes totals with discounts", () => {
    const cart = new Cart().add(keyboard, 2);
    expect(cart.subtotal()).toBe(10000);
    cart.applyDiscount({ kind: "percent", percent: 25 });
    expect(cart.total()).toBe(7500);
    cart.applyDiscount({ kind: "fixed", amount: cents(20000) });
    expect(cart.total()).toBe(0);
  });

  it("rejects non-positive quantities", () => {
    expect(() => new Cart().add(mouse, 0)).toThrow(RangeError);
  });

  it.skipIf(!process.env.OPENIDEA_FAIL)("intentionally fails (set OPENIDEA_FAIL=1)", () => {
    expect(formatMoney(cents(1999))).toBe("$19.98");
  });
});

describe("money", () => {
  it("formats cents", () => {
    expect(formatMoney(cents(123456))).toBe("$1,234.56");
  });
});
