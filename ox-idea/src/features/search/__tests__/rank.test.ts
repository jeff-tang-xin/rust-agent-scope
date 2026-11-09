import { describe, expect, it } from "vitest";
import { byScore, matchText } from "../common/rank";

describe("matchText", () => {
  it("ranks exact > prefix > substring > humps", () => {
    const exact = matchText("Order", "order")!.score;
    const prefix = matchText("OrderService", "order")!.score;
    const sub = matchText("NewOrderService", "order")!.score;
    const hump = matchText("OptimizeImports", "oi")!.score;
    expect(exact).toBeGreaterThan(prefix);
    expect(prefix).toBeGreaterThan(sub);
    expect(sub).toBeGreaterThan(hump);
  });

  it("matches multi-word queries token by token", () => {
    const m = matchText("Optimize Imports", "imp opt");
    expect(m).not.toBeNull();
    expect(m!.indices).toEqual([0, 1, 2, 9, 10, 11]);
    expect(matchText("Optimize Imports", "imp xyz")).toBeNull();
  });

  it("byScore is stable", () => {
    const items = [
      { id: "a", score: 1 },
      { id: "b", score: 2 },
      { id: "c", score: 1 },
    ];
    expect(byScore(items).map((i) => i.id)).toEqual(["b", "a", "c"]);
  });
});
