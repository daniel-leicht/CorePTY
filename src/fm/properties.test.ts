import { describe, expect, it } from "vitest";
import { octal, parseOctal } from "./properties";

describe("octal permissions", () => {
  it("print three digits, four with special bits", () => {
    expect(octal(0o755)).toBe("755");
    expect(octal(0o640)).toBe("640");
    expect(octal(0o4750)).toBe("4750");
    expect(octal(0o1777)).toBe("1777");
    expect(octal(0o7)).toBe("007");
  });

  it("parse only real octal modes", () => {
    expect(parseOctal("755")).toBe(0o755);
    expect(parseOctal(" 4755 ")).toBe(0o4755);
    expect(parseOctal("75")).toBeNull();
    expect(parseOctal("789")).toBeNull();
    expect(parseOctal("rwx")).toBeNull();
    expect(parseOctal("07555")).toBeNull();
  });
});
