import { describe, expect, it } from "vitest";
import { reusableChain, type BackendChain } from "./actions";

describe("reusableChain", () => {
  const chain: BackendChain = {
    counter: 3,
    pos: 10,
    ranges: [
      { from: 8, to: 14 },
      { from: 0, to: 40 },
    ],
  };

  it("reuses the backend chain while the document is unchanged and the selection covers its position", () => {
    expect(reusableChain(chain, 3, { from: 10, to: 10 })).toBe(chain.ranges);
    expect(reusableChain(chain, 3, { from: 8, to: 14 })).toBe(chain.ranges);
  });

  it("asks again after an edit or for a selection elsewhere", () => {
    expect(reusableChain(chain, 4, { from: 10, to: 10 })).toBeNull();
    expect(reusableChain(chain, 3, { from: 11, to: 20 })).toBeNull();
    expect(reusableChain(undefined, 3, { from: 10, to: 10 })).toBeNull();
  });
});
