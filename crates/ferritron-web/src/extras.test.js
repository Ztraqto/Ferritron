// Tests of extras.js with stand-ins for the browser. Run by a test of ferritron-web (needs node);
// `node extras.test.js` runs them by hand.
const fs = require("fs");
const path = require("path");
const assert = require("assert");

const source = fs.readFileSync(path.join(__dirname, "extras.js"), "utf8");

function browser(options = {}) {
  const media = { dark: false, forced: false, reduced: false, ...(options.media ?? {}) };
  const mediaListeners = {};
  const matchMedia = (q) => {
    const key = q.includes("color-scheme") ? "dark" : q.includes("forced") ? "forced" : "reduced";
    return { get matches() { return media[key]; }, addEventListener(type, fn) { if (type === "change") (mediaListeners[key] ??= []).push(fn); } };
  };
  const shared = options.storage ?? new Map();
  const localStorage = {
    getItem: (k) => (shared.has(k) ? shared.get(k) : null),
    setItem: (k, v) => shared.set(k, String(v)),
    removeItem: (k) => shared.delete(k),
    key: (i) => [...shared.keys()][i] ?? null,
    get length() { return shared.size; },
  };
  const windowListeners = {};
  const window = {
    ferritron: { plugin: { id: options.id ?? "dev.example.plugin" }, ...(options.ferritron ?? {}) },
    matchMedia,
    localStorage: options.noStorage ? undefined : localStorage,
    addEventListener(type, fn) { (windowListeners[type] ??= []).push(fn); },
    innerWidth: 800,
    innerHeight: 600,
  };
  if (options.noStorage) Object.defineProperty(window, "localStorage", { get() { throw new Error("denied"); } });
  new Function("window", "document", "CSS", "getComputedStyle", "atob", source)(window, { documentElement: null }, { supports: () => false }, () => ({}), (s) => Buffer.from(s, "base64").toString("binary"));
  return { window, f: window.ferritron, media, changeMedia(...keys) { for (const key of keys) for (const fn of mediaListeners[key] ?? []) fn({}); }, windowListeners, shared };
}

const tests = {
  "the theme follows the media queries and tells about changes"() {
    const b = browser();
    assert.deepStrictEqual({ ...b.f.theme }, { dark: false, highContrast: false, reducedMotion: false, accent: null });
    const seen = [];
    b.f.onThemeChange((t) => seen.push(t));
    b.media.dark = true;
    b.media.reduced = true;
    b.changeMedia("dark", "reduced");
    assert.strictEqual(seen.length, 2, "one notice per changed query");
    assert.strictEqual(seen[0].dark, true);
    assert.strictEqual(b.f.theme.reducedMotion, true);
  },

  "settings keep values, fall back and list keys"() {
    const b = browser();
    const s = b.f.settings;
    assert.strictEqual(s.get("zoom", 1), 1);
    s.set("zoom", 1.5);
    s.set("panel", { open: true, tabs: ["a", "b"] });
    assert.strictEqual(s.get("zoom", 1), 1.5);
    assert.deepStrictEqual(s.get("panel"), { open: true, tabs: ["a", "b"] });
    assert.deepStrictEqual(s.keys(), ["panel", "zoom"]);
    s.remove("zoom");
    assert.strictEqual(s.get("zoom", 7), 7);
    assert.deepStrictEqual(s.keys(), ["panel"]);
  },

  "settings are per plugin and survive a new editor"() {
    const shared = new Map();
    const a = browser({ storage: shared, id: "a.plugin" });
    const b = browser({ storage: shared, id: "b.plugin" });
    a.f.settings.set("language", "ja");
    assert.strictEqual(b.f.settings.get("language", "none"), "none", "another plugin does not see it");
    const again = browser({ storage: shared, id: "a.plugin" });
    assert.strictEqual(again.f.settings.get("language"), "ja", "a later editor of the same plugin does");
  },

  "settings tell about their own changes and those of other editors"() {
    const shared = new Map();
    const a = browser({ storage: shared });
    const seen = [];
    a.f.settings.onChange((k, v) => seen.push([k, v]));
    a.f.settings.set("size", 2);
    a.f.settings.remove("size");
    assert.deepStrictEqual(seen, [["size", 2], ["size", undefined]]);
    // another document wrote: the browser fires `storage` here
    const prefix = [...a.shared.keys()][0] ?? "ferritron.settings.dev.example.plugin.";
    a.windowListeners.storage[0]({ key: "ferritron.settings.dev.example.plugin.theme", newValue: JSON.stringify("dark") });
    a.windowListeners.storage[0]({ key: "unrelated", newValue: "1" });
    a.windowListeners.storage[0]({ key: "ferritron.settings.other.plugin.theme", newValue: "1" });
    assert.deepStrictEqual(seen.at(-1), ["theme", "dark"]);
    assert.strictEqual(seen.length, 3);
    void prefix;
  },

  "settings still work when local storage is refused"() {
    const b = browser({ noStorage: true });
    b.f.settings.set("a", [1, 2]);
    assert.deepStrictEqual(b.f.settings.get("a"), [1, 2]);
    assert.deepStrictEqual(b.f.settings.keys(), ["a"]);
  },

  "in a plugin, the host keeps the settings file and tells of other editors' changes"() {
    const posted = [];
    const b = browser({ ferritron: { _post: (m) => posted.push(m) } });
    b.f._settingsHost({ theme: "dark", zoom: 1.5 });
    assert.strictEqual(b.f.settings.get("theme"), "dark", "the host's values, not local storage");
    assert.deepStrictEqual(b.f.settings.keys(), ["theme", "zoom"]);
    const seen = [];
    b.f.settings.onChange((k, v) => seen.push([k, v]));
    b.f.settings.set("zoom", 2);
    b.f.settings.remove("theme");
    assert.deepStrictEqual(posted, ['settings:["zoom",2]', 'settings:["theme"]']);
    assert.strictEqual(b.shared.size, 0, "nothing in local storage");
    b.f._setting("zoom", 2); // our own change coming back: no second notice
    b.f._setting("language", "ja"); // another editor's
    b.f._setting("zoom", undefined);
    assert.deepStrictEqual(seen, [["zoom", 2], ["theme", undefined], ["language", "ja"], ["zoom", undefined]]);
    assert.deepStrictEqual(b.f.settings.keys(), ["language"]);
  },

  "onStream exists everywhere and decodes base64 floats"() {
    const b = browser();
    const got = [];
    b.f.onStream("spectrum", (v) => got.push(Array.from(v)));
    b.f._stream("spectrum", "AACAPw==");
    b.f._stream("other", "AACAPw==");
    b.f._stream("spectrum", new Float32Array([2, 3]));
    assert.deepStrictEqual(got, [[1], [2, 3]]);
  },

  "what the host already provides is kept"() {
    const onStream = () => "host";
    const openFile = () => Promise.resolve("C:/a.wav");
    const b = browser({ ferritron: { onStream, openFile } });
    assert.strictEqual(b.f.onStream, onStream);
    assert.strictEqual(b.f.openFile, openFile);
    assert.strictEqual(typeof b.f.saveFile, "function");
  },

  async "file dialogs say nothing was chosen where the host has none"() {
    const b = browser();
    assert.strictEqual(await b.f.openFile({}), null);
    assert.strictEqual(await b.f.saveFile({}), null);
  },

  "running twice changes nothing"() {
    const b = browser();
    const settings = b.f.settings;
    new Function("window", "document", "CSS", "getComputedStyle", "atob", source)(b.window, {}, {}, () => ({}), atob);
    assert.strictEqual(b.f.settings, settings);
  },
};

(async () => {
  let failed = 0;
  for (const [name, fn] of Object.entries(tests)) {
    try {
      await fn();
      console.log("ok   " + name);
    } catch (e) {
      failed++;
      console.log("FAIL " + name + "\n     " + (e && e.stack ? e.stack : e));
    }
  }
  console.log(failed ? failed + " failed" : "all passed");
  process.exit(failed ? 1 : 0);
})();
