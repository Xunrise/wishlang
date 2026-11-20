// The hero's wish -> prototype -> implement animation, the install tabs and copy buttons. No tracking, no network.
const editor = document.querySelector(".editor");
const stages = ["wish", "prototype", "implement"];
const show = (stage) => {
  editor.dataset.stage = stage;
  editor.querySelectorAll(".pill").forEach((p) => p.setAttribute("aria-pressed", String(p.dataset.stage === stage)));
};
let timer = matchMedia("(prefers-reduced-motion: reduce)").matches
  ? null
  : setInterval(() => show(stages[(stages.indexOf(editor.dataset.stage) + 1) % stages.length]), 3200);
editor.querySelectorAll(".pill").forEach((p) => p.addEventListener("click", () => {
  clearInterval(timer); // the reader took over
  show(p.dataset.stage);
}));

const tabs = [...document.querySelectorAll("[role=tab]")];
const select = (tab) => {
  tabs.forEach((t) => {
    const on = t === tab;
    t.setAttribute("aria-selected", String(on));
    t.tabIndex = on ? 0 : -1;
    document.getElementById(t.getAttribute("aria-controls")).hidden = !on;
  });
  tab.focus();
};
tabs.forEach((t, i) => {
  t.addEventListener("click", () => select(t));
  t.addEventListener("keydown", (e) => {
    const step = { ArrowRight: 1, ArrowLeft: -1 }[e.key];
    if (step) select(tabs[(i + step + tabs.length) % tabs.length]);
  });
});
// Windows visitors see PowerShell first.
if (/Windows/.test(navigator.userAgent)) select(document.getElementById("t-ps"));

document.querySelectorAll(".copy").forEach((b) => b.addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText(b.previousElementSibling.textContent);
    b.textContent = "Copied";
  } catch {
    b.textContent = "Select and copy";
  }
  setTimeout(() => (b.textContent = "Copy"), 1600);
}));
