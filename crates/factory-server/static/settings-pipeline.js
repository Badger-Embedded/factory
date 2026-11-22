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
  // Each agent id maps to { efforts: [...], default: "..." }, so the effort
  // radios can be redrawn when a stage's agent switches.
  var AGENT_EFFORTS = JSON.parse(form.dataset.agentEfforts || "{}");

  // Each agent id maps to { models: [...], cli: "..." }, so a stage's model
  // chips, suggestions and hint can be redrawn when its agent switches.
  var AGENT_MODELS = JSON.parse(form.dataset.agentModels || "{}");

  // The effort levels of the agent a stage is set to, lowest first.
  function effortsFor(stage) {
    var agent = form.querySelector('input[name="' + stage + '_agent"]:checked');
    var spec = agent && AGENT_EFFORTS[agent.value];
    return spec ? spec.efforts : [];
  }

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
    var index = effortsFor(stage).indexOf(level);
    pips.forEach(function (pip, i) {
      pip.classList.toggle("on", i <= index);
    });
  }

  // Redraws a stage's effort radios for the agent it is now set to. The saved
  // level stays selected when the new agent offers it, otherwise the new agent's
  // default is selected. The node's pips follow through setEffortLabel.
  function redrawEfforts(stage) {
    var agent = form.querySelector('input[name="' + stage + '_agent"]:checked');
    var spec = agent && AGENT_EFFORTS[agent.value];
    var seg = form.querySelector('[data-effort-seg="' + stage + '"]');
    if (!spec || !seg) return;
    var picked = seg.querySelector("input:checked");
    var current = picked ? picked.value : null;
    var selected = spec.efforts.indexOf(current) >= 0 ? current : spec.default;
    seg.textContent = "";
    spec.efforts.forEach(function (level) {
      var label = document.createElement("label");
      var input = document.createElement("input");
      input.type = "radio";
      input.name = stage + "_effort";
      input.value = level;
      input.checked = level === selected;
      var span = document.createElement("span");
      span.textContent = level;
      label.appendChild(input);
      label.appendChild(span);
      seg.appendChild(label);
    });
    setEffortLabel(stage, selected);
  }

  // Redraws a stage's model chips, datalist and hint for the agent it is now
  // set to. A model that was one of the previous agent's suggestions is cleared
  // to "CLI default", since the new agent would refuse it. Any other typed id
  // stays.
  function redrawModels(stage) {
    var agent = form.querySelector('input[name="' + stage + '_agent"]:checked');
    var spec = agent && AGENT_MODELS[agent.value];
    var chips = form.querySelector('[data-model-chips="' + stage + '"]');
    var input = form.querySelector('[data-model-input="' + stage + '"]');
    if (!spec || !chips || !input) return;
    var previous = AGENT_MODELS[chips.dataset.agent];
    var datalist = form.querySelector("#" + input.getAttribute("list"));
    var hint = form.querySelector('[data-model-hint="' + stage + '"]');
    chips.dataset.agent = agent.value;
    chips.textContent = "";
    [""].concat(spec.models).forEach(function (model) {
      var chip = document.createElement("button");
      chip.type = "button";
      chip.className = "chip";
      chip.dataset.model = model;
      chip.dataset.modelFor = stage;
      chip.textContent = model || "CLI default";
      chips.appendChild(chip);
    });
    if (datalist) {
      datalist.textContent = "";
      spec.models.forEach(function (model) {
        var option = document.createElement("option");
        option.value = model;
        datalist.appendChild(option);
      });
    }
    if (hint) {
      hint.textContent =
        "An alias or any model ID. Empty runs the model the installed " + spec.cli + " CLI picks.";
    }
    var value = input.value.trim();
    if (value !== "" && previous && previous.models.indexOf(value) >= 0) {
      input.value = "";
      input.dispatchEvent(new Event("input", { bubbles: true }));
    }
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
    } else if (target.type === "radio" && target.name.endsWith("_agent")) {
      var agentStage = target.name.slice(0, -"_agent".length);
      redrawEfforts(agentStage);
      redrawModels(agentStage);
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
