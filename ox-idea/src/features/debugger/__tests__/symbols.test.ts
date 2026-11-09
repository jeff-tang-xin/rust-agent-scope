import { describe, expect, it } from "vitest";
import { fieldAt, methodAt } from "../breakpoints/symbols";

const java = `package com.shop;

import java.util.List;

public class Orders {
    private int count = 0;
    static final String NAME = "x"; // not a { brace

    public Orders() {
        count = 1;
    }

    @Override
    public List<String> load(int id) {
        int local = 2;
        helper(local);
        return List.of();
    }

    static class Line {
        long total;
        void add(long n) { total += n; }
    }
}
`;

describe("method / field declarations", () => {
  it("names Java methods the way java-debug expects (pkg.Class#method)", () => {
    expect(methodAt("java", java, 13)).toEqual({ name: "com.shop.Orders#load", label: "Orders.load()" });
    expect(methodAt("java", java, 8)).toEqual({ name: "com.shop.Orders#<init>", label: "Orders.Orders()" });
    expect(methodAt("java", java, 21)).toEqual({ name: "com.shop.Orders$Line#add", label: "Line.add()" });
    // Calls and statements are not declarations.
    expect(methodAt("java", java, 14)).toBeNull();
    expect(methodAt("java", java, 15)).toBeNull();
    expect(methodAt("java", java, 16)).toBeNull();
  });

  it("finds Java fields (not locals) as watchpoint ids", () => {
    expect(fieldAt("java", java, 5)).toEqual({ dataId: "com.shop.Orders#count", label: "Orders.count" });
    expect(fieldAt("java", java, 6)).toEqual({ dataId: "com.shop.Orders#NAME", label: "Orders.NAME" });
    expect(fieldAt("java", java, 20)).toEqual({ dataId: "com.shop.Orders$Line#total", label: "Line.total" });
    expect(fieldAt("java", java, 14)).toBeNull();
    expect(fieldAt("java", java, 13)).toBeNull();
    expect(fieldAt("python", "x = 1", 0)).toBeNull();
  });

  it("names functions for debugpy, Delve, LLDB and js-debug", () => {
    expect(methodAt("python", "class A:\n    async def run(self, n):\n        pass", 1)?.name).toBe("run");
    const go =
      "package shop\n\nfunc Parse(s string) int {\n}\nfunc (o *Order) Total() int {\n}\nfunc (o Order) Name() string {\n}";
    expect(methodAt("go", go, 2)?.name).toBe("shop.Parse");
    expect(methodAt("go", go, 4)?.name).toBe("shop.(*Order).Total");
    expect(methodAt("go", go, 6)?.name).toBe("shop.Order.Name");
    expect(methodAt("rust", "pub(crate) async fn parse(text: &str) -> i32 {", 0)?.name).toBe("parse");
    expect(methodAt("c", "static int parse(const char *text) {", 0)?.name).toBe("parse");
    expect(methodAt("c", "    if (x) {", 0)).toBeNull();
    expect(methodAt("c", "    total += parse(buf);", 0)).toBeNull();
    expect(methodAt("typescript", "export async function load(id: number) {", 0)?.name).toBe("load");
    expect(methodAt("plaintext", "def x():", 0)).toBeNull();
  });
});
