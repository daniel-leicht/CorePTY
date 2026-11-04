import { describe, expect, it } from "vitest";
import { commonBits, octal, parseOctal } from "./properties";
import { visibleName } from "./format";

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

  it("tell which bits several entries share", () => {
    // A folder 755 and a script 750: owner rwx and group r-x on in both,
    // group and others write off in both, others read and execute mixed.
    const { on, off } = commonBits([0o755, 0o750]);
    expect(on).toBe(0o750);
    expect(off & 0o777).toBe(0o022);
    expect((on | off) & 0o005).toBe(0);
    // A mode that isn't known decides nothing.
    expect(commonBits([0o644, null])).toEqual({ on: 0, off: 0 });
    expect(commonBits([0o644])).toEqual({ on: 0o644, off: 0o7133 });
  });
});

describe("names shown in questions", () => {
  it("show the controls that reorder text", () => {
    expect(visibleName("invoice‮fdp.exe")).toBe("invoice[U+202E]fdp.exe");
    expect(visibleName("notes.txt")).toBe("notes.txt");
  });
});
