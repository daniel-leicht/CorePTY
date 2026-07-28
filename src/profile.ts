import type { ConnForm } from "./dialog";
import type { SavedSession } from "./ipc";

export type SecretMutation =
  | { kind: "keep" }
  | { kind: "set"; value: string }
  | { kind: "delete" };

/** Build the persisted, non-secret portion of a connection profile. */
export function buildSavedSession(
  form: ConnForm,
  id: string,
  existing?: SavedSession
): SavedSession {
  const folderId = form.folderId ?? null;
  const sameFolder = (existing?.folderId ?? null) === folderId;
  return {
    id,
    name: form.name || form.host,
    kind: form.kind,
    host: form.host,
    port: form.port,
    username: form.kind === "ssh" ? form.username : null,
    authType: form.kind === "ssh" ? form.authType : null,
    keyPath: form.kind === "ssh" && form.authType === "key" ? form.keyPath : null,
    saveSecret: form.kind === "ssh" ? form.saveSecret : false,
    folderId,
    color: existing?.color ?? null,
    order: sameFolder ? (existing?.order ?? null) : null,
  };
}

/** Decide how saving a profile should update its keychain entry. */
export function planSecretMutation(
  form: ConnForm,
  existing?: SavedSession
): SecretMutation {
  const secret =
    form.kind === "ssh"
      ? form.authType === "key"
        ? form.passphrase
        : form.password
      : "";

  if (form.kind === "ssh" && form.saveSecret && secret) {
    return { kind: "set", value: secret };
  }

  if (!existing) {
    return { kind: "keep" };
  }

  const canKeepExisting =
    form.kind === "ssh" &&
    form.saveSecret &&
    existing.kind === "ssh" &&
    existing.saveSecret &&
    existing.authType === form.authType;

  return canKeepExisting ? { kind: "keep" } : { kind: "delete" };
}

/** Fill an untouched secret field immediately before connecting. */
export function applyStoredSecret(form: ConnForm, secret: string): void {
  if (form.authType === "key") form.passphrase = secret;
  else form.password = secret;
}
