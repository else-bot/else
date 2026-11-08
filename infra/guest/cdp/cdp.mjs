// else.bot guest CDP helper for Node.js scripts. Dependency-free.
//
// Drives the computer's own visible Chromium (the one the owner sees over
// VNC) through its DevTools endpoint on guest loopback, 127.0.0.1:9222. The
// endpoint is fixed: this module never connects anywhere else.
//
//   import { connect } from "/usr/local/lib/elsebot/cdp/cdp.mjs";
//   const browser = await connect();              // waits for the browser, takes the lock
//   const page = await browser.page({ create: "http://127.0.0.1:8000/" });
//   await page.fill("#q", "picnic"); await page.click("button");
//   console.log(await page.read("document.title"));
//   await browser.close();                         // releases the lock
//
// Control is serialized: connect() takes the computer's browser lock, which
// computer_browser also takes, so one script (or tool call) drives the
// browser at a time. Nothing that runs in a page is ever sent twice: a
// lost reply raises Uncertain (for actions) or TargetClosed, and the script
// decides. Page code runs with a browser-side time limit, and the tabs a
// script controls are recorded, so if the script is stopped the computer
// closes the tabs it opened and halts code in the ones it attached to.
// Errors: BrowserNotReady, TargetClosed, NavigationTimeout, Uncertain,
// CdpError, BrowserBusy.

import http from "node:http";
import crypto from "node:crypto";
import { spawn } from "node:child_process";
import fs from "node:fs";

export const ENDPOINT = "http://127.0.0.1:9222";
const DESKTOP = "/usr/local/bin/else-desktop";
const SCREEN = "/tmp/.X11-unix/X99";

function startDesktop(timeoutMs) {
  return new Promise((resolve) => {
    const child = spawn(DESKTOP, [], { stdio: "ignore" });
    const timer = setTimeout(() => child.kill(), timeoutMs);
    const done = () => {
      clearTimeout(timer);
      resolve();
    };
    child.on("error", done);
    child.on("exit", done);
  });
}

function canRun(path) {
  try {
    fs.accessSync(path, fs.constants.X_OK);
    return true;
  } catch {
    return false;
  }
}
const LOCK = "/var/lib/elsebot/browser.lock";
export const MAX_EVALUATE_MS = 300000;
const CONTROLLED = "/var/lib/elsebot/browser-controlled.jsonl";

function record(target, created) {
  fs.appendFileSync(CONTROLLED, JSON.stringify({ pid: process.pid, target, created }) + "\n", { mode: 0o600 });
}

function forget() {
  try {
    const kept = fs.readFileSync(CONTROLLED, "utf8").split("\n").filter((l) => {
      try { return l && JSON.parse(l).pid !== process.pid; } catch { return false; }
    });
    fs.writeFileSync(CONTROLLED, kept.map((l) => l + "\n").join(""), { mode: 0o600 });
  } catch {}
}

export class BrowserNotReady extends Error {}
export class BrowserBusy extends Error {}
export class TargetClosed extends Error {}
export class NavigationTimeout extends Error {}
/** The request was sent but no reply came: it may or may not have happened. */
export class Uncertain extends Error {}
export class CdpError extends Error {}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function getJson(path, timeoutMs = 2000) {
  return new Promise((resolve, reject) => {
    const req = http.get(ENDPOINT + path, { timeout: timeoutMs }, (res) => {
      let body = "";
      res.setEncoding("utf8");
      res.on("data", (c) => (body += c));
      res.on("end", () => {
        try {
          resolve(JSON.parse(body));
        } catch (e) {
          reject(e);
        }
      });
    });
    req.on("timeout", () => req.destroy(new Error("timeout")));
    req.on("error", reject);
  });
}

export async function ready({ timeoutMs = 30000 } = {}) {
  const deadline = Date.now() + timeoutMs;
  let last;
  // Only a desktop that never started is started here; a browser that is
  // restarting comes back by itself.
  let started = fs.existsSync(SCREEN) || !canRun(DESKTOP);
  while (Date.now() < deadline) {
    try {
      return await getJson("/json/version");
    } catch (e) {
      last = e;
      if (!started) {
        // The desktop starts on first use.
        started = true;
        await startDesktop(Math.max(1000, deadline - Date.now()));
        continue;
      }
      await sleep(250);
    }
  }
  throw new BrowserNotReady(`the browser did not answer at ${ENDPOINT} within ${timeoutMs} ms (${last?.message ?? "no reply"})`);
}

class Socket {
  static open(url, timeoutMs) {
    const u = new URL(url);
    if (u.hostname !== "127.0.0.1" || u.port !== "9222") {
      throw new CdpError(`refusing a debugger endpoint other than ${ENDPOINT}: ${url}`);
    }
    return new Promise((resolve, reject) => {
      const key = crypto.randomBytes(16).toString("base64");
      const req = http.request({
        host: "127.0.0.1",
        port: 9222,
        path: u.pathname,
        timeout: timeoutMs,
        headers: {
          Connection: "Upgrade",
          Upgrade: "websocket",
          "Sec-WebSocket-Version": "13",
          "Sec-WebSocket-Key": key,
        },
      });
      req.on("timeout", () => req.destroy(new BrowserNotReady("the DevTools connection timed out")));
      req.on("error", reject);
      req.on("response", (res) => reject(new CdpError(`DevTools refused the connection: ${res.statusCode}`)));
      req.on("upgrade", (res, socket, head) => {
        const accept = crypto.createHash("sha1").update(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest("base64");
        if (res.headers["sec-websocket-accept"] !== accept) {
          socket.destroy();
          return reject(new CdpError("bad WebSocket handshake"));
        }
        socket.setTimeout(0);
        resolve(new Socket(socket, head));
      });
      req.end();
    });
  }

  constructor(socket, head) {
    this.socket = socket;
    this.buf = head && head.length ? Buffer.from(head) : Buffer.alloc(0);
    this.parts = [];
    this.onmessage = () => {};
    this.onclose = () => {};
    this.closed = false;
    socket.on("data", (d) => {
      this.buf = Buffer.concat([this.buf, d]);
      this.parse();
    });
    const end = () => {
      if (!this.closed) {
        this.closed = true;
        this.onclose();
      }
    };
    socket.on("close", end);
    socket.on("error", end);
    if (this.buf.length) this.parse();
  }

  parse() {
    for (;;) {
      if (this.buf.length < 2) return;
      const fin = (this.buf[0] & 0x80) !== 0;
      const op = this.buf[0] & 0x0f;
      let len = this.buf[1] & 0x7f;
      let at = 2;
      if (len === 126) {
        if (this.buf.length < 4) return;
        len = this.buf.readUInt16BE(2);
        at = 4;
      } else if (len === 127) {
        if (this.buf.length < 10) return;
        len = Number(this.buf.readBigUInt64BE(2));
        at = 10;
      }
      if (this.buf.length < at + len) return;
      const payload = this.buf.subarray(at, at + len);
      this.buf = this.buf.subarray(at + len);
      if (op === 8) {
        this.socket.end();
        return;
      } else if (op === 9) {
        this.frame(10, payload);
      } else if (op === 1 || op === 0) {
        this.parts.push(Buffer.from(payload));
        if (fin) {
          const text = Buffer.concat(this.parts).toString("utf8");
          this.parts = [];
          this.onmessage(text);
        }
      }
    }
  }

  frame(op, data) {
    const mask = crypto.randomBytes(4);
    const len = data.length;
    const header = len < 126 ? Buffer.from([0x80 | op, 0x80 | len])
      : len < 65536 ? Buffer.from([0x80 | op, 0x80 | 126, len >> 8, len & 255])
      : Buffer.concat([Buffer.from([0x80 | op, 0x80 | 127]), (() => { const b = Buffer.alloc(8); b.writeBigUInt64BE(BigInt(len)); return b; })()]);
    const body = Buffer.alloc(len);
    for (let i = 0; i < len; i++) body[i] = data[i] ^ mask[i % 4];
    this.socket.write(Buffer.concat([header, mask, body]));
  }

  send(text) {
    if (this.closed) throw new TargetClosed("the DevTools connection is closed");
    this.frame(1, Buffer.from(text, "utf8"));
  }

  close() {
    if (!this.closed) {
      try { this.frame(8, Buffer.alloc(0)); } catch {}
      this.socket.end();
    }
  }
}

class Connection {
  constructor(socket) {
    this.socket = socket;
    this.next = 1;
    this.pending = new Map();
    this.listeners = [];
    socket.onmessage = (text) => {
      const msg = JSON.parse(text);
      if (msg.id && this.pending.has(msg.id)) {
        const { resolve, reject, timer } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        clearTimeout(timer);
        if (msg.error) reject(new CdpError(`${msg.error.message} (${msg.error.code})`));
        else resolve(msg.result ?? {});
      } else if (msg.method) {
        for (const l of this.listeners) l(msg);
      }
    };
    socket.onclose = () => {
      for (const [, { reject, timer, mutating, method }] of this.pending) {
        clearTimeout(timer);
        reject(mutating
          ? new Uncertain(`the connection closed before ${method} answered; it may or may not have happened`)
          : new TargetClosed(`the connection closed before ${method} answered`));
      }
      this.pending.clear();
    };
  }

  call(method, params = {}, { timeoutMs = 30000, mutating = true } = {}) {
    const id = this.next++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(mutating
          ? new Uncertain(`${method} did not answer within ${timeoutMs} ms; it may or may not have happened`)
          : new CdpError(`${method} did not answer within ${timeoutMs} ms`));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer, mutating, method });
      try {
        this.socket.send(JSON.stringify({ id, method, params }));
      } catch (e) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(e);
      }
    });
  }

  get closed() {
    return this.socket.closed;
  }

  close() {
    this.socket.close();
  }
}

class Lock {
  static async take(timeoutMs) {
    const child = spawn("/usr/bin/flock", ["-x", "-w", String(Math.ceil(timeoutMs / 1000)), LOCK, "-c", "echo locked; exec cat > /dev/null"], { stdio: ["pipe", "pipe", "ignore"] });
    const got = await new Promise((resolve) => {
      child.stdout.once("data", () => resolve(true));
      child.once("exit", () => resolve(false));
    });
    if (!got) {
      throw new BrowserBusy("another script or computer_browser controls the browser; wait for it or cancel its job");
    }
    child.unref();
    child.stdout.unref?.();
    return { release: () => { try { child.stdin.end(); } catch {} } };
  }
}

export class Page {
  constructor(browser, target, conn) {
    this.browser = browser;
    this.target = target;
    this.conn = conn;
  }

  get id() {
    return this.target.id;
  }

  /** A CDP command on this page. `mutating: false` only changes how a
   * lost reply is reported (TargetClosed instead of Uncertain); nothing is
   * ever resent. After a restart, call reconnect() or open the page again. */
  async call(method, params = {}, options = {}) {
    if (this.conn.closed) {
      throw new TargetClosed("the page connection is closed; call reconnect() or open the page again, and check it before acting");
    }
    return await this.conn.call(method, params, options);
  }

  async evaluate(expression, { mutating = true, timeoutMs = 30000 } = {}) {
    const limit = Math.min(timeoutMs, MAX_EVALUATE_MS);
    const r = await this.call("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true, timeout: limit }, { mutating, timeoutMs: limit + 2000 });
    if (r.exceptionDetails) throw new CdpError(`page script failed: ${r.exceptionDetails.exception?.description ?? r.exceptionDetails.text}`);
    return r.result?.value;
  }

  /** Evaluate an expression you consider side-effect free (a lost reply is
   * TargetClosed or CdpError rather than Uncertain; it is not resent). */
  read(expression, options = {}) {
    return this.evaluate(expression, { ...options, mutating: false });
  }

  text() {
    return this.read("document.body ? document.body.innerText : ''");
  }

  /** Navigate and wait for that navigation's document to load (bounded). */
  async navigate(url, { timeoutMs = 30000 } = {}) {
    const token = crypto.randomBytes(8).toString("hex");
    await this.evaluate(`window.__elseNav = ${JSON.stringify(token)}`, { timeoutMs: 5000 }).catch(() => {});
    let r;
    try {
      r = await this.call("Page.navigate", { url }, { timeoutMs });
    } catch (e) {
      // Chromium answers Page.navigate once the response starts; a server
      // that never answers is a navigation timeout (the tab may be loading).
      if (e instanceof Uncertain && !this.conn.closed) {
        throw new NavigationTimeout(`loading ${url} did not start within ${timeoutMs} ms; the tab may still be loading`);
      }
      throw e;
    }
    if (r.errorText) throw new CdpError(`navigation to ${url} failed: ${r.errorText}`);
    // A new document lacks the marker; a same-document navigation (no
    // loaderId) keeps its document.
    const fresh = r.loaderId ? `window.__elseNav !== ${JSON.stringify(token)} && ` : "";
    await this.waitFor(`${fresh}document.readyState === 'complete'`, { timeoutMs, what: `loading ${url}`, error: NavigationTimeout });
    this.target.url = await this.read("location.href");
  }

  async waitFor(expression, { timeoutMs = 10000, what = expression, error = CdpError } = {}) {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      try {
        if (await this.read(`!!(${expression})`, { timeoutMs: 5000 })) return;
      } catch (e) {
        if (!(e instanceof CdpError)) throw e;
      }
      if (Date.now() > deadline) throw new error(`timed out after ${timeoutMs} ms waiting for ${what}`);
      await sleep(150);
    }
  }

  async fill(selector, value) {
    const ok = await this.evaluate(`(() => { const e = document.querySelector(${JSON.stringify(selector)}); if (!e) return false; e.focus(); e.value = ${JSON.stringify(value)}; e.dispatchEvent(new Event('input', { bubbles: true })); e.dispatchEvent(new Event('change', { bubbles: true })); return true; })()`);
    if (!ok) throw new CdpError(`no element matches ${selector}`);
  }

  async click(selector) {
    const ok = await this.evaluate(`(() => { const e = document.querySelector(${JSON.stringify(selector)}); if (!e) return false; e.click(); return true; })()`);
    if (!ok) throw new CdpError(`no element matches ${selector}`);
  }

  async show() {
    await this.call("Page.bringToFront", {});
  }

  /** Reattach to this same tab (same target ID) after its connection
   * dropped, or fail with TargetClosed: a browser restart gives tabs new
   * IDs, so open the page again with browser.page() and check it. Never
   * repeats an action, never picks another tab. */
  async reconnect({ timeoutMs = 30000 } = {}) {
    await ready({ timeoutMs });
    const found = (await getJson("/json/list")).find((t) => t.type === "page" && t.id === this.target.id);
    if (!found) throw new TargetClosed(`the tab ${this.target.url || this.target.id} is gone`);
    this.target = found;
    this.conn = new Connection(await Socket.open(found.webSocketDebuggerUrl, timeoutMs));
    await this.show();
  }

  close() {
    this.conn.close();
  }
}

export class Browser {
  constructor(lock, timeoutMs) {
    this.lock = lock;
    this.timeoutMs = timeoutMs;
    this.pages = [];
  }

  async targets() {
    return (await getJson("/json/list")).filter((t) => t.type === "page");
  }

  /**
   * Attach to a page: by `id`, `urlPrefix`, or `title`; or `create` a new
   * tab at a URL (shown in the visible window). Without a match and without
   * `create`, the first page tab.
   */
  async page({ id, urlPrefix, title, create } = {}) {
    let target;
    if (create) {
      const version = await ready({ timeoutMs: this.timeoutMs });
      const conn = new Connection(await Socket.open(version.webSocketDebuggerUrl, this.timeoutMs));
      try {
        const { targetId } = await conn.call("Target.createTarget", { url: "about:blank" });
        target = (await this.targets()).find((t) => t.id === targetId);
      } finally {
        conn.close();
      }
      if (!target) throw new TargetClosed("the new tab disappeared");
    } else {
      const pages = await this.targets();
      target = pages.find((t) => (id && t.id === id) || (urlPrefix && t.url.startsWith(urlPrefix)) || (title && t.title === title))
        ?? (!id && !urlPrefix && !title ? pages[0] : undefined);
      if (!target) throw new TargetClosed(`no page matches ${JSON.stringify({ id, urlPrefix, title })}`);
    }
    record(target.id, !!create);
    const page = new Page(this, target, new Connection(await Socket.open(target.webSocketDebuggerUrl, this.timeoutMs)));
    this.pages.push(page);
    await page.show();
    if (create) await page.navigate(create, { timeoutMs: this.timeoutMs });
    return page;
  }

  async close() {
    for (const p of this.pages) p.close();
    forget();
    this.lock?.release();
  }
}

/** Wait for the visible browser and take the browser lock. */
export async function connect({ timeoutMs = 30000, lockTimeoutMs = 60000 } = {}) {
  const lock = await Lock.take(lockTimeoutMs);
  try {
    await ready({ timeoutMs });
  } catch (e) {
    lock.release();
    throw e;
  }
  return new Browser(lock, timeoutMs);
}
