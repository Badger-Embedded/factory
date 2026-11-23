(function () {
  var main = document.querySelector(".job-main[data-job]");
  if (!main) return;
  var key = "factory.job-sections:" + main.getAttribute("data-job");
  var collapsed = new Set();

  try {
    var saved = JSON.parse(localStorage.getItem(key));
    if (Array.isArray(saved) && saved.every(function (name) { return typeof name === "string"; })) {
      saved.forEach(function (name) { collapsed.add(name); });
    }
  } catch (e) {}

  function setOpen(card, open) {
    var body = card.querySelector(":scope > .card-body");
    var toggle = card.querySelector(".section-toggle");
    if (body) body.hidden = !open;
    if (toggle) toggle.setAttribute("aria-expanded", open ? "true" : "false");
  }

  function apply() {
    document.querySelectorAll("[data-section]").forEach(function (card) {
      setOpen(card, !collapsed.has(card.getAttribute("data-section")));
    });
  }

  function save() {
    try {
      if (collapsed.size) localStorage.setItem(key, JSON.stringify(Array.from(collapsed)));
      else localStorage.removeItem(key);
    } catch (e) {}
  }

  function announce(section, open) {
    document.body.dispatchEvent(
      new CustomEvent("factory:section-toggle", { detail: { section: section, open: open } })
    );
  }

  apply();

  // The buttons are replaced on every swap, so the listener sits on the body.
  document.body.addEventListener("click", function (e) {
    var toggle = e.target.closest && e.target.closest("button.section-toggle");
    if (!toggle) return;
    var card = toggle.closest("[data-section]");
    if (!card) return;
    var section = card.getAttribute("data-section");
    var open = collapsed.has(section);
    if (open) {
      collapsed.delete(section);
      setOpen(card, true);
    } else {
      collapsed.add(section);
      announce(section, false);
      setOpen(card, false);
    }
    if (open) announce(section, true);
    save();
  });

  // htmx fires this in the same task as the swap, so no frame shows a
  // collapsed card expanded. It also covers a card that first arrives later.
  document.body.addEventListener("htmx:oobAfterSwap", apply);
})();
