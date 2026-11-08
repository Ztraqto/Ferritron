// A Ferritron plugin as a Web Audio Module (WAM 2.0): the default export is
// the WebAudioModule class hosts load with `import()`:
//
//   const { default: Plugin } = await import("./My Plugin.wam/index.js");
//   const wam = await Plugin.createInstance(hostGroupId, audioContext);
//   source.connect(wam.audioNode); wam.audioNode.connect(audioContext.destination);
//   container.appendChild(await wam.createGui());
//
// The plugin itself is the WebAssembly build of the plugin (plugin.wasm),
// running in an AudioWorklet (processor.js). Parameters are the project
// file's keys ("gain", and "ferritron.bypass", "ferritron.quality").
//
// SPDX-License-Identifier: MIT OR Apache-2.0

import { instantiate } from "./guest.js";
import { createGui as makeGui, destroyGui as dropGui } from "./gui.js";

const BASE = new URL(".", import.meta.url);
const API_VERSION = "2.0.0";

let loaded = null;
/** The descriptor, the compiled module and a main-thread instance for text and the description; once per page. */
function load() {
  loaded ??= (async () => {
    const descriptor = await (await fetch(new URL("descriptor.json", BASE))).json();
    const response = fetch(new URL("plugin.wasm", BASE));
    const module = await WebAssembly.compileStreaming(response).catch(async () =>
      WebAssembly.compile(await (await fetch(new URL("plugin.wasm", BASE))).arrayBuffer()));
    const language = (globalThis.navigator?.language ?? "en");
    const meta = await instantiate(module, language);
    return { descriptor, module, meta, language, description: meta.describe() };
  })();
  return loaded;
}

const worklets = new WeakMap();
/** Adds processor.js to the context's AudioWorklet once. */
function addProcessor(audioContext, name) {
  let added = worklets.get(audioContext);
  if (!added) {
    added = audioContext.audioWorklet.addModule(new URL(`processor.js?name=${encodeURIComponent(name)}`, BASE));
    worklets.set(audioContext, added);
  }
  return added;
}

/** WamParameterInfo for one parameter of the description. */
function parameterInfo(p) {
  const type = { float: "float", int: "int", bool: "boolean", enum: "choice" }[p.kind];
  return {
    id: p.key, label: p.name, type, defaultValue: p.default, minValue: p.min, maxValue: p.max,
    discreteStep: p.kind === "float" ? 0 : 1, exponent: 0, choices: p.choices ?? [], units: p.unit,
    normalize: (v) => (p.max > p.min ? (v - p.min) / (p.max - p.min) : 0),
    denormalize: (n) => p.min + n * (p.max - p.min),
    valueString: (v) => String(v),
  };
}

class FerritronNode extends AudioWorkletNode {
  constructor(wam, loaded, state) {
    const instrument = loaded.descriptor.isInstrument;
    const params = loaded.description.params.filter((p) => p.key !== "ferritron.mcp");
    super(wam.audioContext, wam.processorName, {
      numberOfInputs: instrument ? 0 : 1,
      numberOfOutputs: 1,
      outputChannelCount: [2],
      processorOptions: {
        groupId: wam.groupId, moduleId: wam.moduleId, instanceId: wam.instanceId, instrument,
        module: loaded.module, language: loaded.language, state,
        ids: Object.fromEntries(params.map((p) => [p.key, p.id])),
      },
    });
    this.module = wam;
    this.groupId = wam.groupId;
    this.moduleId = wam.moduleId;
    this.instanceId = wam.instanceId;
    this.meta = loaded.meta;
    this.params = params;
    this.byKey = Object.fromEntries(params.map((p) => [p.key, p]));
    this.calls = new Map();
    this.next = 0;
    this.latency = 0;
    this.ready = new Promise((resolve, reject) => {
      this.port.onmessage = ({ data }) => {
        if (data.type === "ready") {
          this.latency = data.latency;
          resolve();
        } else if (data.type === "error") {
          reject(new Error(data.message));
        } else if (data.type === "reply") {
          this.calls.get(data.id)?.(data);
          this.calls.delete(data.id);
        }
      };
    });
  }

  call(op, data = {}) {
    const id = this.next++;
    return new Promise((resolve) => {
      this.calls.set(id, resolve);
      this.port.postMessage({ op, id, ...data });
    });
  }

  // --- WamNode ---------------------------------------------------------------

  async getParameterInfo(...ids) {
    const keys = ids.length ? ids : this.params.map((p) => p.key);
    return Object.fromEntries(keys.filter((k) => this.byKey[k]).map((k) => [k, parameterInfo(this.byKey[k])]));
  }

  async getParameterValues(normalized = false, ...ids) {
    const { values } = await this.call("get");
    const byId = new Map(values);
    const keys = ids.length ? ids : this.params.map((p) => p.key);
    return Object.fromEntries(keys.filter((k) => this.byKey[k]).map((k) => {
      const p = this.byKey[k];
      const n = byId.get(p.id) ?? 0;
      return [k, { id: k, value: normalized ? n : this.meta.toPlain(p.id, n), normalized }];
    }));
  }

  async setParameterValues(values) {
    const list = Object.values(values).filter((v) => this.byKey[v.id]).map((v) => {
      const p = this.byKey[v.id];
      return [p.id, v.normalized ? v.value : this.meta.toNormalized(p.id, v.value)];
    });
    await this.call("set", { values: list });
  }

  async getState() {
    const { state } = await this.call("getState");
    return { ferritron: state };
  }

  async setState(state) {
    const bytes = state?.ferritron;
    if (bytes) await this.call("setState", { state: bytes });
  }

  async getCompensationDelay() {
    const { latency } = await this.call("latency");
    return latency;
  }

  scheduleEvents(...events) {
    this.call("events", { events });
  }

  clearEvents() {
    this.call("clear");
  }

  connectEvents(toId, output) {
    this.call("connect", { to: toId, output });
  }

  disconnectEvents(toId, output) {
    this.call("disconnect", { to: toId, output });
  }

  destroy() {
    this.call("destroy");
    this.disconnect();
  }
}

export default class FerritronWam {
  static isWebAudioModuleConstructor = true;

  static async createInstance(groupId, audioContext, initialState) {
    return new this(groupId, audioContext).initialize(initialState);
  }

  constructor(groupId, audioContext) {
    this.isWebAudioModule = true;
    this.groupId = groupId;
    this.audioContext = audioContext;
    this.initialized = false;
    this._descriptor = null;
    this.instanceId = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
  }

  get descriptor() { return this._descriptor; }
  get moduleId() { return this._descriptor?.identifier; }
  get name() { return this._descriptor?.name; }
  get vendor() { return this._descriptor?.vendor; }
  get processorName() { return `ferritron:${this.moduleId}`; }

  async initialize(state) {
    const l = await load();
    this._descriptor = l.descriptor;
    this.description = l.description;
    this.meta = l.meta;
    await addProcessor(this.audioContext, this.processorName);
    this.audioNode = new FerritronNode(this, l, state?.ferritron ?? null);
    await this.audioNode.ready;
    this.initialized = true;
    return this;
  }

  /** The editor: the plugin's own web editor (gui/), or one made from the parameters. */
  async createGui() {
    return makeGui(this, new URL("gui/", BASE));
  }

  destroyGui(gui) {
    dropGui(gui);
  }
}

export { API_VERSION };
