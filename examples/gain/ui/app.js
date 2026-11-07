// The page talks to the plugin only through `window.ferritron`.
const p = ferritron.params.gain;
document.querySelector("h1").textContent = ferritron.t("ui-title");
const slider = document.getElementById("slider");
const value = document.getElementById("value");
document.getElementById("name").textContent = p.name;
slider.min = p.min;
slider.max = p.max;

const show = (param) => {
  slider.value = param.value;
  value.textContent = `${param.text} ${param.unit}`;
};
show(p);

// A drag is one gesture: begin, any number of sets, end.
slider.addEventListener("pointerdown", () => ferritron.begin("gain"));
slider.addEventListener("input", () => ferritron.set("gain", slider.value));
slider.addEventListener("pointerup", () => ferritron.end("gain"));
slider.addEventListener("dblclick", () => ferritron.edit("gain", 0));

// Values changed by automation, the host or this page.
ferritron.onChange((key, param) => key === "gain" && show(param));
