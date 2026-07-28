import { describe, expect, it } from "vitest";
import { SgrDimFilter } from "./dimfix";

const encoder = new TextEncoder();
const decoder = new TextDecoder();
const filter = (input: string): string =>
  decoder.decode(new SgrDimFilter().feed(encoder.encode(input)));

describe("SgrDimFilter", () => {
  it("does not reset an explicit foreground on SGR 22", () => {
    const input = "\u001b[31mred\u001b[1mbold\u001b[22mred";
    expect(filter(input)).toBe(input);
  });

  it("restores the default foreground after replacing default dim", () => {
    expect(filter("\u001b[2mdim\u001b[22mnormal")).toBe(
      "\u001b[38;2;110;113;121mdim\u001b[22;39mnormal"
    );
  });

  it("does not mistake truecolor components for the dim attribute", () => {
    const input = "\u001b[38;2;2;20;22mcolor\u001b[22m";
    expect(filter(input)).toBe(input);
  });

  it("preserves an explicit foreground that replaces default dim", () => {
    expect(filter("\u001b[2mdim\u001b[31mred\u001b[22m")).toBe(
      "\u001b[38;2;110;113;121mdim\u001b[31;2mred\u001b[22m"
    );
  });

  it("reapplies the replacement when a dim explicit color returns to default", () => {
    expect(filter("\u001b[31m\u001b[2mred\u001b[39mdefault\u001b[22m")).toBe(
      "\u001b[31m\u001b[2mred\u001b[38;2;110;113;121mdefault\u001b[22;39m"
    );
  });

  it("handles an SGR split across input chunks", () => {
    const instance = new SgrDimFilter();
    const first = instance.feed(encoder.encode("\u001b["));
    const second = instance.feed(encoder.encode("2mdim"));
    expect(decoder.decode(first)).toBe("");
    expect(decoder.decode(second)).toBe("\u001b[38;2;110;113;121mdim");
  });
});
