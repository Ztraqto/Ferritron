// The AudioWorklet side of a Ferritron WAM: one plugin instance per node,
// processing in place in guest memory. Loaded by index.js as
// `processor.js?name=<processor name>`.
//
// WAM 2.0: implements WamProcessor (scheduleEvents, emitEvents, clearEvents,
// getCompensationDelay, destroy) and joins the host's WamEnv when there is
// one (`globalThis.webAudioModules`), so events from other WAMs arrive.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

import { Block, instantiate } from "./guest.js";

// AudioWorkletGlobalScope has no URL class.
const NAME = decodeURIComponent(/[?&]name=([^&#]*)/.exec(import.meta.url)?.[1] ?? "ferritron");

class FerritronProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const o = options.processorOptions;
    this.groupId = o.groupId;
    this.moduleId = o.moduleId;
    this.instanceId = o.instanceId;
    this.instrument = o.instrument;
    this.ids = o.ids; // key -> parameter id
    this.alive = true;
    this.ready = false;
    this.queue = []; // WAM events, by time
    this.pending = []; // [id, normalized] to apply at the next block
    this.port.onmessage = (e) => this.message(e.data);
    try {
      this.env = globalThis.webAudioModules;
      this.env?.addWam?.(this);
    } catch (e) {
      this.env = null;
    }
    instantiate(o.module, o.language).then((guest) => {
      this.guest = guest;
      this.instance = guest.create();
      if (!this.instance) throw new Error("ferritron: the plugin could not create an instance");
      guest.activate(this.instance, sampleRate, 1024);
      this.block = new Block(guest, 2);
      if (o.state) guest.loadState(this.instance, o.state);
      this.ready = true;
      this.port.postMessage({ type: "ready", latency: guest.latency(this.instance) });
    }).catch((e) => this.port.postMessage({ type: "error", message: String(e?.message ?? e) }));
  }

  reply(id, data) {
    this.port.postMessage({ type: "reply", id, ...data });
  }

  message(m) {
    const g = this.guest;
    switch (m.op) {
      case "set":
        for (const [id, normalized] of m.values) {
          if (this.ready) g.setParam(this.instance, id, normalized);
          this.pending.push([id, normalized]);
        }
        return this.reply(m.id, {});
      case "get": {
        if (!this.ready) return this.reply(m.id, { values: [] });
        const values = Object.values(this.ids).map((id) => [id, g.getParam(this.instance, id)]);
        return this.reply(m.id, { values });
      }
      case "getState":
        return this.reply(m.id, { state: this.ready ? g.saveState(this.instance) : null });
      case "setState":
        return this.reply(m.id, { ok: this.ready && g.loadState(this.instance, m.state) });
      case "latency":
        return this.reply(m.id, { latency: this.ready ? g.latency(this.instance) : 0 });
      case "events":
        this.scheduleEvents(...m.events);
        return this.reply(m.id, {});
      case "clear":
        this.clearEvents();
        return this.reply(m.id, {});
      case "connect":
        this.env?.connectEvents?.(this.groupId, this.instanceId, m.to, m.output);
        return this.reply(m.id, {});
      case "disconnect":
        this.env?.disconnectEvents?.(this.groupId, this.instanceId, m.to, m.output);
        return this.reply(m.id, {});
      case "destroy":
        this.destroy();
        return this.reply(m.id, {});
    }
  }

  // --- WamProcessor -------------------------------------------------------

  getCompensationDelay() {
    return this.ready ? this.guest.latency(this.instance) : 0;
  }

  scheduleEvents(...events) {
    for (const e of events) this.queue.push(e);
    this.queue.sort((a, b) => (a.time ?? 0) - (b.time ?? 0));
  }

  emitEvents(...events) {
    this.env?.emitEvents?.(this, ...events);
  }

  clearEvents() {
    this.queue = [];
  }

  destroy() {
    this.alive = false;
    try {
      this.env?.removeWam?.(this);
    } catch (e) {}
    if (this.ready) this.guest.destroy(this.instance);
    this.ready = false;
  }

  // --- Processing -----------------------------------------------------------

  /** Events of this block as plugin events, sorted by offset. */
  takeEvents(frames) {
    const events = this.pending.map(([id, normalized]) => ({ offset: 0, param: [id, normalized] }));
    this.pending = [];
    const end = currentTime + frames / sampleRate;
    while (this.queue.length && (this.queue[0].time ?? 0) < end) {
      const e = this.queue.shift();
      const offset = Math.min(frames - 1, Math.max(0, Math.round(((e.time ?? 0) - currentTime) * sampleRate)));
      if (e.type === "wam-automation") {
        const id = this.ids[e.data.id] ?? Number(e.data.id);
        if (id === undefined || Number.isNaN(id)) continue;
        const normalized = e.data.normalized ? e.data.value : this.guest.toNormalized(id, e.data.value);
        events.push({ offset, param: [id, normalized] });
      } else if (e.type === "wam-midi") {
        const [status, key, velocity] = e.data.bytes;
        const kind = status & 0xf0;
        if (kind === 0x90 && velocity > 0) events.push({ offset, note: [true, key, status & 15, velocity / 127] });
        else if (kind === 0x80 || kind === 0x90) events.push({ offset, note: [false, key, status & 15, velocity / 127] });
      }
    }
    return events.sort((a, b) => a.offset - b.offset);
  }

  process(inputs, outputs) {
    const out = outputs[0];
    if (!this.ready || !out?.length) return this.alive;
    const frames = out[0].length;
    const block = this.block;
    for (let c = 0; c < 2; c++) {
      const dst = block.channel(c, frames);
      const src = this.instrument ? null : inputs[0]?.[Math.min(c, (inputs[0]?.length ?? 1) - 1)];
      if (src) dst.set(src);
      else dst.fill(0);
    }
    for (const e of this.takeEvents(frames)) {
      if (e.param) block.param(e.offset, e.param[0], e.param[1]);
      else block.note(e.offset, ...e.note);
    }
    const ok = block.process(this.instance, frames);
    for (let c = 0; c < out.length; c++) {
      if (ok) out[c].set(block.channel(Math.min(c, 1), frames));
      else out[c].fill(0);
    }
    return this.alive;
  }
}

registerProcessor(NAME, FerritronProcessor);
