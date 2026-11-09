import { describe, expect, it } from "vitest";
import { projectInitials } from "./projectColor";

describe("projectInitials", () => {
  it("follows IDEA's AvatarUtils.initials", () => {
    expect(projectInitials("idea")).toBe("I");
    expect(projectInitials("MyProject")).toBe("MP");
    expect(projectInitials("OpenIDEA")).toBe("OI");
    expect(projectInitials("py-demo")).toBe("PD");
    expect(projectInitials("web demo")).toBe("WD");
    expect(projectInitials("a.b.c")).toBe("AC");
    expect(projectInitials("123abc")).toBe("A");
    expect(projectInitials("")).toBe("?");
  });
});
