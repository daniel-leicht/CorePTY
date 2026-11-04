import { describe, expect, it } from "vitest";
import { learn, providerBadge, providerLabel } from "./remote";

const opened = (fs: string, label: string) =>
  learn(
    { fs, connectionId: fs, label, home: { fs, path: "p:", display: "/" }, notice: null },
    async () => null
  );

describe("provider labels", () => {
  it("say LOCAL for this machine", () => {
    expect(providerLabel("local")).toBe("LOCAL");
    expect(providerBadge("local")).toBe("LOCAL");
  });

  it("name the server, the badge in short", () => {
    opened("sftp:a", "deploy@edge-01.example.net");
    expect(providerLabel("sftp:a")).toBe("SFTP deploy@edge-01.example.net");
    expect(providerBadge("sftp:a")).toBe("SFTP deploy@edge-01");
    opened("sftp:b", "root@10.0.0.12:2222");
    expect(providerBadge("sftp:b")).toBe("SFTP root@10.0.0.12");
    opened("sftp:c", "me@[::1]:2222");
    expect(providerBadge("sftp:c")).toBe("SFTP me@[::1]");
    expect(providerBadge("sftp:unknown")).toBe("SFTP");
  });
});
