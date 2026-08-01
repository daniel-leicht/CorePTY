import { spawn } from "node:child_process";
import { copyFile, mkdir, mkdtemp, rm, stat, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const docsDir = join(repoRoot, "docs");
const port = 14321;
const origin = `http://127.0.0.1:${port}`;
const themes = [
  "corepty-dark",
  "corepty-light",
  "dracula",
  "nord",
  "solarized-dark",
  "bbs",
  "synapse",
  "starbase",
];

const edge = findEdge();

async function main() {
  if (typeof WebSocket === "undefined") {
    throw new Error("Screenshot capture requires Node.js 22 or newer.");
  }

  const workingDir = await mkdtemp(join(tmpdir(), "corepty-screenshots-"));
  const renderDir = join(workingDir, "renders");
  const browserProfile = join(workingDir, "edge-profile");
  const vite = spawn(
    process.execPath,
    [join(repoRoot, "node_modules", "vite", "bin", "vite.js"), "--host", "127.0.0.1", "--port", String(port), "--strictPort"],
    { cwd: repoRoot, windowsHide: true, stdio: ["ignore", "ignore", "pipe"] }
  );
  let viteError = "";
  vite.stderr.on("data", (chunk) => {
    viteError += chunk.toString();
  });

  let browser;
  let client;
  try {
    await waitForServer(vite, () => viteError);
    await mkdir(renderDir, { recursive: true });
    await mkdir(browserProfile, { recursive: true });

    const debugPort = await getFreePort();
    browser = spawn(
      edge,
      [
        "--headless=new",
        "--no-first-run",
        "--disable-default-apps",
        "--disable-extensions",
        "--disable-background-networking",
        "--disable-background-timer-throttling",
        "--disable-backgrounding-occluded-windows",
        "--disable-renderer-backgrounding",
        "--disable-webgl",
        "--force-color-profile=srgb",
        "--force-device-scale-factor=1",
        "--hide-scrollbars",
        "--remote-allow-origins=*",
        `--remote-debugging-port=${debugPort}`,
        `--user-data-dir=${browserProfile}`,
        "--window-size=1280,800",
        "about:blank",
      ],
      { cwd: repoRoot, windowsHide: true, stdio: ["ignore", "ignore", "pipe"] }
    );
    let browserError = "";
    browser.stderr.on("data", (chunk) => {
      browserError += chunk.toString();
    });

    const target = await waitForBrowser(debugPort, browser, () => browserError);
    client = await CdpClient.connect(target.webSocketDebuggerUrl);
    await client.send("Page.enable");
    await client.send("Runtime.enable");
    await client.send("Emulation.setDeviceMetricsOverride", {
      width: 1280,
      height: 800,
      deviceScaleFactor: 1,
      mobile: false,
      screenWidth: 1280,
      screenHeight: 800,
    });

    for (const theme of themes) {
      const output = join(renderDir, `theme-${theme}.png`);
      await capture(client, theme, output, "terminal");
      const { size } = await stat(output);
      if (size < 10_000) throw new Error(`Screenshot looks incomplete: ${output} (${size} bytes)`);
      process.stdout.write(`captured ${theme} (${Math.round(size / 1024)} KiB)\n`);
    }

    const fileManagerOutput = join(renderDir, "file-manager.png");
    await capture(client, "corepty-dark", fileManagerOutput, "files");
    const { size: fileManagerSize } = await stat(fileManagerOutput);
    if (fileManagerSize < 10_000) {
      throw new Error(`Screenshot looks incomplete: ${fileManagerOutput} (${fileManagerSize} bytes)`);
    }
    process.stdout.write(`captured file manager (${Math.round(fileManagerSize / 1024)} KiB)\n`);

    // Stop the watcher before publishing images into docs/ so it cannot reload
    // a page during a later capture.
    vite.kill();
    await waitForProcessExit(vite, 2_000);
    await mkdir(docsDir, { recursive: true });
    for (const theme of themes) {
      await copyFile(join(renderDir, `theme-${theme}.png`), join(docsDir, `theme-${theme}.png`));
    }
    await copyFile(join(renderDir, "theme-corepty-dark.png"), join(docsDir, "screenshot.png"));
    await copyFile(fileManagerOutput, join(docsDir, "file-manager.png"));
    process.stdout.write("updated docs/screenshot.png\n");
  } finally {
    if (client) {
      try {
        await client.send("Browser.close", {}, 2_000);
      } catch {
        // The browser may already be gone after a capture failure.
      }
      client.close();
    }
    if (browser?.exitCode === null) {
      await waitForProcessExit(browser, 1_000);
      if (browser.exitCode === null) browser.kill();
    }
    if (vite.exitCode === null) vite.kill();
    await removeWithRetry(workingDir);
  }
}

function findEdge() {
  const configured = process.env.COREPTY_EDGE;
  const candidates = [
    configured,
    "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
    "C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe",
  ].filter(Boolean);
  const found = candidates.find((candidate) => existsSync(candidate));
  if (!found) {
    throw new Error("Microsoft Edge was not found. Set COREPTY_EDGE to its executable path.");
  }
  return found;
}

async function waitForServer(vite, readError) {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    if (vite.exitCode !== null) {
      throw new Error(`Vite exited before capture started.\n${readError()}`);
    }
    try {
      const response = await fetch(`${origin}/scripts/showcase.html`);
      if (response.ok) return;
    } catch {
      // The server is still starting.
    }
    await delay(100);
  }
  throw new Error(`Timed out waiting for Vite.\n${readError()}`);
}

async function getFreePort() {
  return new Promise((resolvePort, rejectPort) => {
    const server = createServer();
    server.once("error", rejectPort);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      const freePort = typeof address === "object" && address ? address.port : 0;
      server.close((error) => (error ? rejectPort(error) : resolvePort(freePort)));
    });
  });
}

async function waitForBrowser(debugPort, browser, readError) {
  const endpoint = `http://127.0.0.1:${debugPort}/json/list`;
  for (let attempt = 0; attempt < 150; attempt += 1) {
    if (browser.exitCode !== null) {
      throw new Error(`Edge exited before DevTools was ready.\n${readError()}`);
    }
    try {
      const targets = await (await fetch(endpoint)).json();
      const page = targets.find((target) => target.type === "page");
      if (page?.webSocketDebuggerUrl) return page;
    } catch {
      // DevTools is still starting.
    }
    await delay(100);
  }
  throw new Error(`Timed out waiting for Edge DevTools.\n${readError()}`);
}

async function capture(client, theme, output, scene) {
  const url = `${origin}/scripts/showcase.html?theme=${encodeURIComponent(theme)}&scene=${encodeURIComponent(scene)}`;
  const loaded = client.waitForEvent("Page.loadEventFired", 15_000);
  const navigation = await client.send("Page.navigate", { url });
  if (navigation.errorText) throw new Error(`Could not load ${theme}: ${navigation.errorText}`);
  await loaded;

  const deadline = Date.now() + 15_000;
  let lastStage = "unknown";
  let ready = false;
  while (Date.now() < deadline) {
    const evaluation = await client.send("Runtime.evaluate", {
      expression: `(() => ({
        ready: document.documentElement.dataset.showcaseReady === "true",
        stage: document.documentElement.dataset.captureStage ?? "unknown",
        error: document.body.textContent?.startsWith("Screenshot fixture failed:")
          ? document.body.textContent
          : ""
      }))()`,
      returnByValue: true,
    });
    const state = evaluation.result?.value;
    if (state?.error) throw new Error(state.error);
    if (state?.ready) {
      ready = true;
      break;
    }
    lastStage = state?.stage ?? lastStage;
    await delay(100);
  }
  if (!ready) {
    throw new Error(`Timed out rendering ${theme} (last stage: ${lastStage}).`);
  }

  await client.send("Runtime.evaluate", {
    expression: "new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)))",
    awaitPromise: true,
  });
  const screenshot = await client.send("Page.captureScreenshot", {
    format: "png",
    fromSurface: true,
    captureBeyondViewport: false,
  });
  await writeFile(output, Buffer.from(screenshot.data, "base64"));
}

function delay(ms) {
  return new Promise((resolveDelay) => setTimeout(resolveDelay, ms));
}

async function waitForProcessExit(child, timeout) {
  if (child.exitCode !== null) return;
  await Promise.race([
    new Promise((resolveExit) => child.once("exit", resolveExit)),
    delay(timeout),
  ]);
}

async function removeWithRetry(path) {
  for (let attempt = 0; attempt < 5; attempt += 1) {
    try {
      await rm(path, { recursive: true, force: true });
      return;
    } catch (error) {
      if (attempt === 4) throw error;
      await delay(150);
    }
  }
}

class CdpClient {
  static async connect(url) {
    const socket = new WebSocket(url);
    await new Promise((resolveOpen, rejectOpen) => {
      const timer = setTimeout(() => rejectOpen(new Error("Timed out connecting to Edge DevTools.")), 10_000);
      socket.addEventListener(
        "open",
        () => {
          clearTimeout(timer);
          resolveOpen();
        },
        { once: true }
      );
      socket.addEventListener(
        "error",
        () => {
          clearTimeout(timer);
          rejectOpen(new Error("Could not connect to Edge DevTools."));
        },
        { once: true }
      );
    });
    return new CdpClient(socket);
  }

  constructor(socket) {
    this.socket = socket;
    this.nextId = 1;
    this.pending = new Map();
    this.eventWaiters = new Map();
    socket.addEventListener("message", (event) => this.onMessage(event.data));
    socket.addEventListener("close", () => this.rejectPending(new Error("Edge DevTools disconnected.")));
  }

  send(method, params = {}, timeout = 15_000) {
    const id = this.nextId++;
    return new Promise((resolveCommand, rejectCommand) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        rejectCommand(new Error(`DevTools command timed out: ${method}`));
      }, timeout);
      this.pending.set(id, { resolve: resolveCommand, reject: rejectCommand, timer });
      this.socket.send(JSON.stringify({ id, method, params }));
    });
  }

  waitForEvent(method, timeout = 15_000) {
    return new Promise((resolveEvent, rejectEvent) => {
      const timer = setTimeout(() => {
        const waiters = this.eventWaiters.get(method) ?? [];
        this.eventWaiters.set(method, waiters.filter((waiter) => waiter.resolve !== resolveEvent));
        rejectEvent(new Error(`DevTools event timed out: ${method}`));
      }, timeout);
      const waiters = this.eventWaiters.get(method) ?? [];
      waiters.push({ resolve: resolveEvent, reject: rejectEvent, timer });
      this.eventWaiters.set(method, waiters);
    });
  }

  close() {
    this.socket.close();
  }

  onMessage(data) {
    const message = JSON.parse(typeof data === "string" ? data : Buffer.from(data).toString("utf8"));
    if (message.id) {
      const pending = this.pending.get(message.id);
      if (!pending) return;
      this.pending.delete(message.id);
      clearTimeout(pending.timer);
      if (message.error) pending.reject(new Error(message.error.message));
      else pending.resolve(message.result ?? {});
      return;
    }

    if (!message.method) return;
    const waiters = this.eventWaiters.get(message.method);
    if (!waiters?.length) return;
    this.eventWaiters.delete(message.method);
    for (const waiter of waiters) {
      clearTimeout(waiter.timer);
      waiter.resolve(message.params ?? {});
    }
  }

  rejectPending(error) {
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.pending.clear();
    for (const waiters of this.eventWaiters.values()) {
      for (const waiter of waiters) {
        clearTimeout(waiter.timer);
        waiter.reject(error);
      }
    }
    this.eventWaiters.clear();
  }
}

await main();
