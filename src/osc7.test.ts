import { describe, expect, it } from "vitest";
import { parseOsc7 } from "./osc7";

describe("OSC 7 working folders", () => {
  it("take the path, decoded, whatever the host", () => {
    expect(parseOsc7("file://edge-01/var/www")).toBe("/var/www");
    expect(parseOsc7("file:///home/deploy/my%20app")).toBe("/home/deploy/my app");
    expect(parseOsc7("file://box/")).toBe("/");
  });

  it("ignore anything else", () => {
    expect(parseOsc7("http://example.com/x")).toBeNull();
    expect(parseOsc7("file://host")).toBeNull();
    expect(parseOsc7("file://host/%E0%A4%A")).toBeNull();
    expect(parseOsc7("")).toBeNull();
  });
});
