import { describe, expect, it } from "vitest";
import { Claims } from "./claims";

/** A backend that counts references like `sftp_retain` / `sftp_close`. */
function backend(open: string[]) {
  const users = new Map(open.map((fs) => [fs, 1]));
  const calls: string[] = [];
  return {
    users,
    calls,
    retain: async (fs: string) => {
      calls.push(`retain ${fs}`);
      if (!users.has(fs)) return false;
      users.set(fs, users.get(fs)! + 1);
      return true;
    },
    close: (fs: string) => {
      calls.push(`close ${fs}`);
      const left = (users.get(fs) ?? 0) - 1;
      if (left > 0) users.set(fs, left);
      else users.delete(fs);
    },
  };
}

const remote = (fs: string) => fs !== "local";

describe("a tab's claims on remote filesystems", () => {
  it("survive swapping panels: the new claim comes before the old goes", async () => {
    const server = backend([]);
    const claims = new Claims(server, remote);
    // Opening the server: connect took a reference (the backend's 1).
    server.users.set("sftp:a", 1);
    const right = claims.adopt("sftp:a");
    // Ctrl+U: the left panel goes to the server, then the right one leaves it.
    const left = (await claims.claim("sftp:a"))!;
    right();
    expect(server.users.get("sftp:a")).toBe(1);
    expect(server.calls).not.toContain("close sftp:a");
    // The last panel leaves: the tab lets go.
    left();
    expect(server.users.has("sftp:a")).toBe(false);
    expect(claims.held().size).toBe(0);
  });

  it("go back to a server another holder keeps open, but not to a closed one", async () => {
    const server = backend(["sftp:a"]); // a job keeps it open
    const claims = new Claims(server, remote);
    const back = await claims.claim("sftp:a");
    expect(back).not.toBeNull();
    expect(server.users.get("sftp:a")).toBe(2);
    expect(await claims.claim("sftp:gone")).toBeNull();
    back!();
    back!(); // giving back twice counts once
    expect(server.users.get("sftp:a")).toBe(1);
  });

  it("hold one reference per tab, whatever the number of claims", async () => {
    const server = backend(["sftp:a"]);
    const claims = new Claims(server, remote);
    const [one, two] = await Promise.all([claims.claim("sftp:a"), claims.claim("sftp:a")]);
    expect(server.users.get("sftp:a")).toBe(2); // the job's and this tab's
    // Opening it again gives the extra reference back.
    server.users.set("sftp:a", server.users.get("sftp:a")! + 1);
    const three = claims.adopt("sftp:a");
    expect(server.users.get("sftp:a")).toBe(2);
    one!();
    two!();
    three();
    expect(server.users.get("sftp:a")).toBe(1);
  });

  it("let go of everything when the tab closes, once", async () => {
    const server = backend(["sftp:a"]);
    const claims = new Claims(server, remote);
    const held = (await claims.claim("sftp:a"))!;
    claims.dispose();
    expect(server.users.get("sftp:a")).toBe(1);
    held();
    expect(server.users.get("sftp:a")).toBe(1);
    // A server opened after the tab closed is given back at once.
    server.users.set("sftp:b", 1);
    claims.adopt("sftp:b")();
    expect(server.users.has("sftp:b")).toBe(false);
  });

  it("need nothing for this machine", async () => {
    const server = backend([]);
    const claims = new Claims(server, remote);
    const local = await claims.claim("local");
    expect(local).not.toBeNull();
    local!();
    expect(server.calls).toEqual([]);
  });
});
