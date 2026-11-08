// A Ferritron plugin built for WebAssembly (wasm32-wasip1), driven through
// its C ABI table (ferritron_abi.h) like crates/ferritron-wasm does natively.
// Used on the main thread (parameter text, the description) and in the
// AudioWorklet (processing). No dependencies; runs in both scopes.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

const ENOSYS = 52;
const EBADF = 8;

// wasm32 layout of ferritron_plugin_api: function pointers are table indices.
const API = {
  info: 8, create: 12, destroy: 16, paramCount: 20, paramInfo: 24, getParam: 28, setParam: 32,
  toPlain: 36, toNormalized: 40, formatParam: 44, parseParam: 48, activate: 52, process: 56,
  saveState: 60, loadState: 64, getExtension: 68,
};
export const EVENT_SIZE = 24;
export const EVENT_PARAM = 0;
export const EVENT_NOTE_ON = 1;
export const EVENT_NOTE_OFF = 2;
export const MAX_EVENTS = 512;
export const MAX_FRAMES = 128 * 8;

// AudioWorkletGlobalScope has no TextEncoder / TextDecoder: UTF-8 by hand there.
const encoder = typeof TextEncoder !== "undefined" ? new TextEncoder() : {
  encode(text) {
    const out = [];
    for (const ch of text) {
      const c = ch.codePointAt(0);
      if (c < 0x80) out.push(c);
      else if (c < 0x800) out.push(0xc0 | (c >> 6), 0x80 | (c & 63));
      else if (c < 0x10000) out.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
      else out.push(0xf0 | (c >> 18), 0x80 | ((c >> 12) & 63), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
    }
    return Uint8Array.from(out);
  },
};
const decoder = typeof TextDecoder !== "undefined" ? new TextDecoder() : {
  decode(bytes) {
    let out = "";
    for (let i = 0; i < bytes.length;) {
      const b = bytes[i++];
      let c = b;
      if (b >= 0xf0) c = ((b & 7) << 18) | ((bytes[i++] & 63) << 12) | ((bytes[i++] & 63) << 6) | (bytes[i++] & 63);
      else if (b >= 0xe0) c = ((b & 15) << 12) | ((bytes[i++] & 63) << 6) | (bytes[i++] & 63);
      else if (b >= 0xc0) c = ((b & 31) << 6) | (bytes[i++] & 63);
      out += String.fromCodePoint(c);
    }
    return out;
  },
};

/** The WASI functions a Rust guest imports; everything else answers ENOSYS. */
function wasi(getMemory, environment) {
  const env = Object.entries(environment).map(([k, v]) => encoder.encode(`${k}=${v}\0`));
  let line = "";
  const view = () => new DataView(getMemory().buffer);
  const bytes = () => new Uint8Array(getMemory().buffer);
  const known = {
    environ_sizes_get(count, size) {
      view().setUint32(count, env.length, true);
      view().setUint32(size, env.reduce((n, e) => n + e.length, 0), true);
      return 0;
    },
    environ_get(environ, buf) {
      for (const e of env) {
        view().setUint32(environ, buf, true);
        bytes().set(e, buf);
        environ += 4;
        buf += e.length;
      }
      return 0;
    },
    args_sizes_get(count, size) {
      view().setUint32(count, 0, true);
      view().setUint32(size, 0, true);
      return 0;
    },
    args_get() { return 0; },
    fd_write(fd, iovs, count, written) {
      let total = 0;
      for (let i = 0; i < count; i++) {
        const ptr = view().getUint32(iovs + i * 8, true);
        const len = view().getUint32(iovs + i * 8 + 4, true);
        line += decoder.decode(bytes().slice(ptr, ptr + len));
        total += len;
      }
      view().setUint32(written, total, true);
      let nl;
      while ((nl = line.indexOf("\n")) >= 0) {
        (fd === 2 ? console.warn : console.log)(`[ferritron] ${line.slice(0, nl)}`);
        line = line.slice(nl + 1);
      }
      return 0;
    },
    random_get(buf, len) {
      const b = bytes();
      for (let i = 0; i < len; i++) b[buf + i] = (Math.random() * 256) | 0;
      return 0;
    },
    clock_time_get(id, precision, out) {
      const ms = typeof performance !== "undefined" ? performance.timeOrigin + performance.now() : Date.now();
      view().setBigUint64(out, BigInt(Math.round(ms * 1e6)), true);
      return 0;
    },
    clock_res_get(id, out) {
      view().setBigUint64(out, 1000n, true);
      return 0;
    },
    fd_prestat_get() { return EBADF; },
    fd_close() { return 0; },
    sched_yield() { return 0; },
    proc_exit(code) { throw new Error(`ferritron: the plugin exited (${code})`); },
  };
  return new Proxy(known, { get: (t, name) => t[name] ?? (() => ENOSYS) });
}

/** A string at `ptr` (a ferritron_str: pointer and length). */
function strAt(memory, at) {
  const v = new DataView(memory.buffer);
  const ptr = v.getUint32(at, true);
  const len = v.getUint32(at + 4, true);
  return decoder.decode(new Uint8Array(memory.buffer, ptr, len).slice());
}

/**
 * One instance of the module. `language` becomes FERRITRON_LANGUAGE, which
 * picks the plugin's translations.
 */
export async function instantiate(module, language) {
  let memory = null;
  const imports = {};
  for (const imp of WebAssembly.Module.imports(module)) {
    if (imp.kind !== "function") continue;
    imports[imp.module] ??= imp.module.startsWith("wasi")
      ? wasi(() => memory, { FERRITRON_LANGUAGE: language || "en" })
      : new Proxy({}, { get: () => () => ENOSYS });
  }
  const instance = await WebAssembly.instantiate(module, imports);
  const ex = instance.exports;
  memory = ex.memory;
  if (!ex.__indirect_function_table) throw new Error("ferritron: the module exports no function table (build with -Clink-arg=--export-table)");
  ex._initialize?.();
  return new Guest(ex);
}

export class Guest {
  constructor(ex) {
    this.ex = ex;
    this.memory = ex.memory;
    this.table = ex.__indirect_function_table;
    this.api = ex.ferritron_plugin_entry();
    if (this.u32(this.api + 4) !== 1) throw new Error("ferritron: the module was built for another Ferritron ABI version");
    const fn = (offset) => this.table.get(this.u32(this.api + offset));
    for (const [name, offset] of Object.entries(API)) if (name !== "info") this[`_${name}`] = fn(offset);
    // Memory for strings, blocks and events, allocated once.
    this.scratch = ex.ferritron_wasm_alloc(64 * 1024);
  }

  u32(at) { return new DataView(this.memory.buffer).getUint32(at, true); }
  f64(at) { return new DataView(this.memory.buffer).getFloat64(at, true); }

  /** Writes `bytes` into guest memory at `at`. */
  put(at, bytes) { new Uint8Array(this.memory.buffer).set(bytes, at); }

  /** A NUL-terminated copy of `text` in guest memory (allocated, never freed: for extension ids). */
  cString(text) {
    const b = encoder.encode(`${text}\0`);
    const at = this.ex.ferritron_wasm_alloc(b.length);
    this.put(at, b);
    return at;
  }

  extension(id) {
    const at = this._getExtension(this.cString(id));
    return at === 0 ? null : at;
  }

  /** The plugin described (`ferritron.describe.v1`): keys, kinds, ranges, localized names, messages. */
  describe() {
    const ext = this.extension("ferritron.describe.v1");
    if (!ext) throw new Error("ferritron: the plugin is too old for the web (no ferritron.describe.v1)");
    const describe = this.table.get(this.u32(ext));
    const len = describe(0, 0);
    const buf = this.ex.ferritron_wasm_alloc(len);
    describe(buf, len);
    return JSON.parse(decoder.decode(new Uint8Array(this.memory.buffer, buf, len).slice()));
  }

  paramCount() { return this._paramCount(); }

  /** id, name, unit, steps, defaultNormalized, flags of parameter `index`. */
  paramInfo(index) {
    const out = this.scratch;
    if (!this._paramInfo(index, out)) return null;
    return {
      id: this.u32(out), name: strAt(this.memory, out + 4), unit: strAt(this.memory, out + 12),
      steps: this.u32(out + 20), defaultNormalized: this.f64(out + 24), flags: this.u32(out + 32),
    };
  }

  toPlain(id, normalized) { return this._toPlain(id, normalized); }
  toNormalized(id, plain) { return this._toNormalized(id, plain); }

  format(id, normalized) {
    const buf = this.scratch, cap = 256;
    const len = this._formatParam(id, normalized, buf, cap);
    return len < 0 ? "" : decoder.decode(new Uint8Array(this.memory.buffer, buf, len).slice());
  }

  /** The normalized value of display text, or null. */
  parse(id, text) {
    const bytes = encoder.encode(text).slice(0, 1024);
    const header = this.scratch, result = this.scratch + 16, string = this.scratch + 64;
    this.put(string, bytes);
    const v = new DataView(this.memory.buffer);
    v.setUint32(header, string, true);
    v.setUint32(header + 4, bytes.length, true);
    // wasm32's C ABI passes the string either by pointer or as two values.
    const ok = this._parseParam.length === 3
      ? this._parseParam(id, header, result)
      : this._parseParam(id, string, bytes.length, result);
    return ok ? this.f64(result) : null;
  }

  create() { return this._create(); }
  destroy(instance) { this._destroy(instance); }
  getParam(instance, id) { return this._getParam(instance, id); }
  setParam(instance, id, normalized) { return !!this._setParam(instance, id, normalized); }
  activate(instance, sampleRate, maxFrames) { return !!this._activate(instance, sampleRate, maxFrames); }

  latency(instance) {
    const ext = this.extension("ferritron.latency.v1");
    return ext ? this.table.get(this.u32(ext))(instance) : 0;
  }

  saveState(instance) {
    const size = this._saveState(instance, 0, 0);
    const buf = this.ex.ferritron_wasm_alloc(size);
    const written = this._saveState(instance, buf, size);
    return written === size ? new Uint8Array(this.memory.buffer, buf, size).slice() : null;
  }

  loadState(instance, bytes) {
    const buf = this.ex.ferritron_wasm_alloc(bytes.length);
    this.put(buf, bytes);
    return !!this._loadState(instance, buf, bytes.length);
  }
}

/**
 * Processing memory of one instance: channel pointers, events, audio,
 * allocated once in the guest. `process` runs the block in place.
 */
export class Block {
  constructor(guest, channels) {
    this.guest = guest;
    this.channels = channels;
    this.pointers = guest.ex.ferritron_wasm_alloc(channels * 4);
    this.events = guest.ex.ferritron_wasm_alloc(MAX_EVENTS * EVENT_SIZE + 8);
    this.events = (this.events + 7) & ~7;
    this.audio = guest.ex.ferritron_wasm_alloc(channels * MAX_FRAMES * 4 + 16);
    this.audio = (this.audio + 15) & ~15;
    const v = new DataView(guest.memory.buffer);
    for (let c = 0; c < channels; c++) v.setUint32(this.pointers + c * 4, this.audio + c * MAX_FRAMES * 4, true);
    this.count = 0;
  }

  /** The audio of channel `c` in guest memory (a fresh view: memory may grow). */
  channel(c, frames) {
    return new Float32Array(this.guest.memory.buffer, this.audio + c * MAX_FRAMES * 4, frames);
  }

  param(offset, id, normalized) {
    if (this.count >= MAX_EVENTS) return;
    const at = this.events + this.count++ * EVENT_SIZE;
    const v = new DataView(this.guest.memory.buffer);
    v.setUint32(at, offset, true);
    v.setUint32(at + 4, EVENT_PARAM, true);
    v.setUint32(at + 8, id, true);
    v.setFloat64(at + 16, normalized, true);
  }

  note(offset, on, key, channel, velocity) {
    if (this.count >= MAX_EVENTS) return;
    const at = this.events + this.count++ * EVENT_SIZE;
    const v = new DataView(this.guest.memory.buffer);
    v.setUint32(at, offset, true);
    v.setUint32(at + 4, on ? EVENT_NOTE_ON : EVENT_NOTE_OFF, true);
    v.setInt32(at + 8, -1, true);
    v.setInt16(at + 12, channel, true);
    v.setInt16(at + 14, key, true);
    v.setFloat32(at + 16, velocity, true);
  }

  /** Runs the plugin on the block; false if it failed (the shim outputs silence). */
  process(instance, frames) {
    const ok = this.guest._process(instance, this.pointers, this.channels, frames, this.events, this.count, 0);
    this.count = 0;
    return !!ok;
  }
}
