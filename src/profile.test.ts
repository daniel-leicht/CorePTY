import { describe, expect, it } from "vitest";
import type { ConnForm } from "./dialog";
import type { SavedSession } from "./ipc";
import {
  applyStoredSecret,
  buildSavedSession,
  planSecretMutation,
} from "./profile";

const existing: SavedSession = {
  id: "session-1",
  name: "Production",
  kind: "ssh",
  host: "example.com",
  port: 22,
  username: "alice",
  authType: "password",
  keyPath: null,
  saveSecret: true,
  folderId: "folder-1",
  color: "blue",
  order: 4,
};

function form(overrides: Partial<ConnForm> = {}): ConnForm {
  return {
    id: existing.id,
    name: existing.name,
    kind: "ssh",
    host: existing.host,
    port: 22,
    username: "alice",
    authType: "password",
    password: "",
    keyPath: "",
    passphrase: "",
    saveSecret: true,
    folderId: "folder-1",
    ...overrides,
  };
}

describe("connection profile persistence", () => {
  it("preserves an existing secret when an unrelated field is edited", () => {
    expect(planSecretMutation(form({ name: "Renamed" }), existing)).toEqual({
      kind: "keep",
    });
  });

  it("updates a secret when the user enters a replacement", () => {
    expect(planSecretMutation(form({ password: "replacement" }), existing)).toEqual({
      kind: "set",
      value: "replacement",
    });
  });

  it("deletes the old secret when storage is disabled or auth type changes", () => {
    expect(planSecretMutation(form({ saveSecret: false }), existing)).toEqual({
      kind: "delete",
    });
    expect(planSecretMutation(form({ authType: "key" }), existing)).toEqual({
      kind: "delete",
    });
  });

  it("retains extension metadata and resets order only when moving folders", () => {
    expect(buildSavedSession(form({ name: "Renamed" }), existing.id, existing)).toMatchObject({
      name: "Renamed",
      color: "blue",
      order: 4,
    });
    expect(
      buildSavedSession(form({ folderId: "folder-2" }), existing.id, existing).order
    ).toBeNull();
  });

  it("hydrates the active authentication field only", () => {
    const passwordForm = form();
    applyStoredSecret(passwordForm, "stored-password");
    expect(passwordForm.password).toBe("stored-password");
    expect(passwordForm.passphrase).toBe("");

    const keyForm = form({ authType: "key" });
    applyStoredSecret(keyForm, "stored-passphrase");
    expect(keyForm.passphrase).toBe("stored-passphrase");
    expect(keyForm.password).toBe("");
  });
});
