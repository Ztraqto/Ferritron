// Every instance of the plugin in this process, from the plugin's messages.
const t = (key, args) => ferritron.t(key, args);
document.getElementById("title").textContent = t("ui-title");
document.getElementById("note-label").textContent = t("ui-note");
const note = document.getElementById("note");
const list = document.getElementById("list");

const showTrack = (instance) => {
  const name = instance?.track?.name;
  document.getElementById("track").textContent = name ? t("ui-on-track", { name }) : t("ui-no-track");
};
showTrack(ferritron.instance);
ferritron.onInstanceChange(showTrack);

note.addEventListener("change", () => ferritron.send({ kind: "note", text: note.value }));

let dragging = false;
ferritron.onMessage((message) => {
  if (message.kind !== "overview" || dragging) return;
  list.replaceChildren(...message.instances.map((i) => {
    const row = document.createElement("li");
    row.className = i.me ? "me" : "";
    const dot = document.createElement("span");
    dot.className = "dot";
    if (i.color) dot.style.background = `rgb(${i.color.join(",")})`;
    const name = document.createElement("div");
    name.textContent = i.track ?? t("ui-no-track");
    const small = document.createElement("small");
    small.textContent = i.note ?? "";
    name.append(small);
    const slider = document.createElement("input");
    slider.type = "range";
    slider.min = -60;
    slider.max = 12;
    slider.step = 0.1;
    slider.value = i.level;
    const value = document.createElement("output");
    value.textContent = `${Number(i.level).toFixed(1)} dB`;
    slider.addEventListener("pointerdown", () => (dragging = true));
    slider.addEventListener("input", () => {
      value.textContent = `${Number(slider.value).toFixed(1)} dB`;
      ferritron.send({ kind: "level", id: i.id, db: Number(slider.value) });
    });
    slider.addEventListener("pointerup", () => (dragging = false));
    row.append(dot, name, slider, value);
    if (i.me && document.activeElement !== note) note.value = i.note ?? "";
    return row;
  }));
});

// Other instances change on their own: ask for the overview now and then.
ferritron.send({ kind: "refresh" });
setInterval(() => ferritron.send({ kind: "refresh" }), 500);
