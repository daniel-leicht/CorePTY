import { describe, expect, it } from "vitest";
import { parseOsc7 } from "./osc7";

describe("OSC 7 working folders", () => {
  it("take the host and the decoded path", () => {
    expect(parseOsc7("file://edge-01/var/www")).toEqual({ host: "edge-01", path: "/var/www" });
    expect(parseOsc7("file:///home/deploy/my%20app")).toEqual({ host: "", path: "/home/deploy/my app" });
    expect(parseOsc7("FILE://Box/")).toEqual({ host: "box", path: "/" });
  });

  it("ignore anything else", () => {
    expect(parseOsc7("http://example.com/x")).toBeNull();
    expect(parseOsc7("file://host")).toBeNull();
    expect(parseOsc7("file://host/%E0%A4%A")).toBeNull();
    expect(parseOsc7("file://host/a%00b")).toBeNull();
    expect(parseOsc7("")).toBeNull();
  });
});
