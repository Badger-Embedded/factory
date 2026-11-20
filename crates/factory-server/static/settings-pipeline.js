// The pipeline on a project's settings page. Selecting a stage, a gate or
// merge shows its panel without a reload and updates the `?stage=` value in
// the address and the form's action, so a save comes back to the same panel.
// The model buttons fill the stage's model field, "Use the default" sets a
// gate back to the factory default, and the nodes follow the fields as they
// change. Without this script each node is a link that reloads the page with
// its panel open, and the buttons stay hidden.
(function () {
  document.documentElement.classList.add("pipeline-js");
  var form = document.querySelector(".pipeline-form");
  if (!form) return;
  var EFFORTS = ["low", "medium", "high", "xhigh", "max"];

  function select(part) {
    form.querySelectorAll("[data-part]").forEach(function (node) {
      if (node.dataset.part === part) node.setAttribute("aria-current", "step");
      else node.removeAttribute("aria-current");
    });
    form.querySelectorAll(".pipe-panel").forEach(function (panel) {
      panel.hidden = panel.dataset.panel !== part;
    });
    var url = new URL(form.action, window.location.href);
    url.searchParams.set("stage", part);
    form.action = url.pathname + url.search;
    history.replaceState(null, "", url.pathname + url.search);
  }

  function setModelLabel(stage) {
    var label = form.querySelector('[data-model-label="' + stage + '"]');
    var input = form.querySelector('[data-model-input="' + stage + '"]');
    if (!label || !input) return;
    var review = stage === "review" && !form.querySelector('[name="llm_review"]').checked;
    var model = input.value.trim();
    label.textContent = review ? "Off" : model || "CLI default";
    label.classList.toggle("dflt", review || model === "");
    form.querySelector('[data-part="' + stage + '"]').classList.toggle("off", review);
  }

  function setEffortLabel(stage, level) {
    var pips = form.querySelectorAll('[data-effort-label="' + stage + '"] i');
    var index = EFFORTS.indexOf(level);
    pips.forEach(function (pip, i) {
      pip.classList.toggle("on", i <= index);
    });
  }

  function setGateNode(box) {
    var node = form.querySelector('[data-gate-node="' + box.dataset.gate + '"]');
    var changed = box.checked !== (box.dataset.default === "on");
    node.classList.toggle("on", box.checked);
    node.classList.toggle("changed", changed);
    var note = box.closest(".pipe-panel").querySelector(".gate-changed");
    if (note) note.hidden = !changed;
  }

  form.addEventListener("click", function (event) {
    var node = event.target.closest("[data-part]");
    if (node) {
      event.preventDefault();
      select(node.dataset.part);
      return;
    }
    var chip = event.target.closest("[data-model-for]");
    if (chip) {
      var input = form.querySelector('[data-model-input="' + chip.dataset.modelFor + '"]');
      input.value = chip.dataset.model;
      input.dispatchEvent(new Event("input", { bubbles: true }));
      return;
    }
    var reset = event.target.closest("[data-gate-reset]");
    if (reset) {
      var box = form.querySelector('[data-gate="' + reset.dataset.gateReset + '"]');
      var on = box.dataset.default === "on";
      if (box.checked !== on) {
        box.checked = on;
        box.dispatchEvent(new Event("change", { bubbles: true }));
      }
    }
  });

  form.addEventListener("input", function (event) {
    if (event.target.dataset.modelInput) setModelLabel(event.target.dataset.modelInput);
  });

  form.addEventListener("change", function (event) {
    var target = event.target;
    if (target.type === "radio" && target.name.endsWith("_effort")) {
      setEffortLabel(target.name.slice(0, -"_effort".length), target.value);
    } else if (target.dataset.gate) {
      setGateNode(target);
    } else if (target.name === "llm_review") {
      setModelLabel("review");
    } else if (target.name === "auto_merge") {
      form.querySelector("[data-merge-label]").textContent = target.checked
        ? "Automatic"
        : "You, on GitHub";
    } else {
      var stage = /^mcp\.(.+)\.stage\.([a-z]+)$/.exec(target.name);
      if (stage) {
        var label = form.querySelector('[data-server-label="' + stage[1] + "." + stage[2] + '"]');
        if (label) label.hidden = !target.checked;
      }
    }
  });
})();
