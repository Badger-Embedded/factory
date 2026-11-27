use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

const DOM: &str = r#"
const fs = require("fs");
class El {
  constructor(tag, attrs = {}) {
    this.tag = tag; this.attrs = {}; this.children = []; this.parent = null;
    this.listeners = {}; this.checked = false; this.hidden = false;
    this.classList = { add() {}, toggle() {} };
    for (const [k, v] of Object.entries(attrs)) this.setAttr(k, v);
    const self = this;
    this.dataset = new Proxy({}, {
      get: (_, k) => self.attrs["data-" + String(k).replace(/[A-Z]/g, (c) => "-" + c.toLowerCase())],
      set: (_, k, v) => { self.attrs["data-" + String(k).replace(/[A-Z]/g, (c) => "-" + c.toLowerCase())] = v; return true; },
    });
  }
  setAttr(k, v) { this.attrs[k] = v; }
  getAttribute(k) { return k in this.attrs ? this.attrs[k] : null; }
  setAttribute(k, v) { this.attrs[k] = v; }
  removeAttribute(k) { delete this.attrs[k]; }
  appendChild(c) { c.parent = this; this.children.push(c); return c; }
  set textContent(v) { this.children = []; this._text = v; }
  get textContent() { return this._text ?? ""; }
  set name(v) { this.attrs.name = v; } get name() { return this.attrs.name; }
  set value(v) { this.attrs.value = v; } get value() { return this.attrs.value ?? ""; }
  set type(v) { this.attrs.type = v; } get type() { return this.attrs.type; }
  set className(v) { this.attrs.class = v; }
  addEventListener(t, f) { (this.listeners[t] ||= []).push(f); }
  dispatchEvent(e) {
    e.target = e.target || this;
    for (let n = this; n; n = n.parent) for (const f of n.listeners[e.type] || []) f(e);
  }
  closest(sel) { for (let n = this; n; n = n.parent) if (n.matches(sel)) return n; return null; }
  matches(sel) {
    const re = /^([a-z]+)?(#[\w-]+)?((?:\.[\w-]+)*)((?:\[[^\]]+\])*)(:checked)?$/;
    const m = re.exec(sel);
    if (!m) throw new Error("selector " + sel);
    if (m[1] && m[1] !== this.tag) return false;
    if (m[2] && this.attrs.id !== m[2].slice(1)) return false;
    for (const c of (m[3] || "").split(".").filter(Boolean)) if (!(this.attrs.class || "").split(" ").includes(c)) return false;
    for (const a of (m[4] || "").match(/\[[^\]]+\]/g) || []) {
      const [, k, v] = /^\[([\w-]+)(?:="(.*)")?\]$/.exec(a);
      if (!(k in this.attrs)) return false;
      if (v !== undefined && this.attrs[k] !== v) return false;
    }
    return !m[5] || this.checked;
  }
  querySelectorAll(sel) {
    const out = [];
    const walk = (n) => { for (const c of n.children) { if (c.matches(sel)) out.push(c); walk(c); } };
    walk(this);
    return out;
  }
  querySelector(sel) { return this.querySelectorAll(sel)[0] || null; }
}
global.Event = function (type) { this.type = type; };
const doc = { documentElement: new El("html"), createElement: (t) => new El(t) };
global.document = doc;
global.window = { location: { href: "http://x/" } };
global.history = { replaceState() {} };

const SEARCH = [["openai/gpt-5", "OpenAI: GPT-5"], ["x-ai/grok-4", "xAI: Grok 4"]];
for (let i = 0; i < 28; i++) SEARCH.push(["acme/model-" + i, "Acme Model " + i]);
const AGENTS = {
  claude: { models: ["fable", "opus"], hint: "An alias or any model ID. Empty runs the model the installed claude CLI picks.", required: false },
  copilot: { models: ["claude-sonnet-5.5", "gpt-6.2-sol"], hint: "An alias or any model ID. Empty runs the model the installed copilot CLI picks.", required: false },
  local: { models: ["gemma-4-26B", "Llama-3.2-1B"], hint: "Models served by http://gfx1201:9931. A Local stage needs a model.", required: true },
  openrouter: { models: [], hint: "Search 30 OpenRouter models.", required: true, search: SEARCH },
};
const EFFORTS = {
  claude: { efforts: ["low", "medium", "high", "xhigh", "max"], default: "high" },
  copilot: { efforts: ["low", "medium", "high", "xhigh"], default: "medium" },
  local: { efforts: ["low", "medium", "high", "xhigh"], default: "medium" },
  openrouter: { efforts: ["low", "medium", "high", "xhigh"], default: "medium" },
};
const form = new El("form", { class: "pipeline-form", "data-agent-models": JSON.stringify(AGENTS), "data-agent-efforts": JSON.stringify(EFFORTS) });
function stage(name, agent, model) {
  const radios = {};
  for (const id of Object.keys(AGENTS)) {
    const r = form.appendChild(new El("input", { type: "radio", name: name + "_agent", value: id }));
    r.checked = id === agent;
    radios[id] = r;
  }
  const seg = form.appendChild(new El("div", { "data-effort-seg": name }));
  for (const level of EFFORTS[agent].efforts) {
    const label = seg.appendChild(new El("label"));
    const r = label.appendChild(new El("input", { type: "radio", name: name + "_effort", value: level }));
    r.checked = level === "high";
  }
  const chips = form.appendChild(new El("div", { "data-model-chips": name, "data-agent": agent }));
  for (const m of [""].concat(AGENTS[agent].models)) chips.appendChild(new El("button", { "data-model": m, "data-model-for": name }));
  const input = form.appendChild(new El("input", { type: "text", name, list: "model-suggestions-" + name, value: model, "data-model-input": name }));
  const list = form.appendChild(new El("datalist", { id: "model-suggestions-" + name }));
  const results = form.appendChild(new El("ul", { "data-model-results": name }));
  results.hidden = true;
  const hint = form.appendChild(new El("p", { "data-model-hint": name }));
  hint.textContent = "old";
  return { radios, seg, chips, input, list, hint, results };
}
const baseQuery = El.prototype.querySelectorAll;
form.querySelectorAll = function (sel) { return sel.endsWith(" i") ? [] : baseQuery.call(this, sel); };
doc.querySelector = (sel) => (form.matches(sel) ? form : null);
global.inputs = [];
form.addEventListener("input", (e) => global.inputs.push(e.target.name));
function pick(s, id) {
  for (const [k, r] of Object.entries(s.radios)) r.checked = k === id;
  s.radios[id].dispatchEvent({ type: "change", target: s.radios[id] });
}
function state(s) {
  return {
    model: s.input.value,
    chips: s.chips.children.map((c) => c.dataset.model),
    labels: s.chips.children.map((c) => c.textContent),
    list: s.list.children.map((o) => o.value),
    hint: s.hint.textContent,
    results: s.results.children.map((r) => r.dataset.modelResult),
    resultsHidden: s.results.hidden,
    placeholder: s.input.placeholder,
    efforts: s.seg.children.map((l) => l.children[0].value),
    effort: (s.seg.children.find((l) => l.children[0].checked) || { children: [{}] }).children[0].value,
  };
}
const load = () => new Function(fs.readFileSync(process.argv[1], "utf8"))();
"#;

fn run(harness: &str) -> Value {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static/settings-pipeline.js");
    let output = Command::new("node")
        .args(["-e", &format!("{DOM}{harness}")])
        .arg(script)
        .output()
        .expect("node runs; the dev shell provides it");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn switching_the_agent_redraws_the_models_and_clears_a_suggestion_of_the_old_agent() {
    let out = run(r#"
const implement = stage("implement", "claude", "fable");
load();
pick(implement, "copilot");
console.log(JSON.stringify({ after: state(implement), inputs: global.inputs }));
"#);
    let after = &out["after"];
    assert_eq!(after["model"], "");
    assert_eq!(
        after["chips"],
        serde_json::json!(["", "claude-sonnet-5.5", "gpt-6.2-sol"])
    );
    assert_eq!(after["labels"][0], "CLI default");
    assert_eq!(
        after["list"],
        serde_json::json!(["claude-sonnet-5.5", "gpt-6.2-sol"])
    );
    assert_eq!(
        after["hint"],
        "An alias or any model ID. Empty runs the model the installed copilot CLI picks."
    );
    assert_eq!(out["inputs"], serde_json::json!(["implement"]));
}

#[test]
fn switching_the_agent_keeps_a_custom_model_and_leaves_other_stages_alone() {
    let out = run(r#"
const implement = stage("implement", "claude", "my-custom-model");
const plan = stage("plan", "claude", "opus");
load();
pick(implement, "copilot");
console.log(JSON.stringify({ implement: state(implement), plan: state(plan), inputs: global.inputs }));
"#);
    assert_eq!(out["implement"]["model"], "my-custom-model");
    assert_eq!(out["implement"]["chips"][1], "claude-sonnet-5.5");
    assert_eq!(out["plan"]["model"], "opus");
    assert_eq!(
        out["plan"]["chips"],
        serde_json::json!(["", "fable", "opus"])
    );
    assert_eq!(out["plan"]["hint"], "old");
    assert_eq!(out["inputs"], serde_json::json!([]));
}

#[test]
fn switching_back_compares_with_the_agent_the_chips_were_drawn_for() {
    let out = run(r#"
const implement = stage("implement", "claude", "");
load();
pick(implement, "copilot");
implement.input.value = "gpt-6.2-sol";
pick(implement, "claude");
console.log(JSON.stringify({ after: state(implement) }));
"#);
    assert_eq!(out["after"]["model"], "");
    assert_eq!(
        out["after"]["chips"],
        serde_json::json!(["", "fable", "opus"])
    );
}

#[test]
fn switching_to_local_shows_the_server_models_and_its_efforts_and_switching_back_restores_them() {
    let out = run(r#"
const implement = stage("implement", "claude", "fable");
load();
pick(implement, "local");
const local = state(implement);
implement.input.value = "gemma-4-26B";
pick(implement, "claude");
console.log(JSON.stringify({ local, back: state(implement) }));
"#);
    let local = &out["local"];
    assert_eq!(
        local["chips"],
        serde_json::json!(["gemma-4-26B", "Llama-3.2-1B"])
    );
    assert_eq!(local["list"], local["chips"]);
    assert_eq!(
        local["hint"],
        "Models served by http://gfx1201:9931. A Local stage needs a model."
    );
    assert_eq!(local["placeholder"], "Model ID");
    assert_eq!(
        local["efforts"],
        serde_json::json!(["low", "medium", "high", "xhigh"])
    );
    assert_eq!(local["effort"], "high");
    let back = &out["back"];
    assert_eq!(back["model"], "");
    assert_eq!(back["chips"], serde_json::json!(["", "fable", "opus"]));
    assert_eq!(back["placeholder"], "CLI default");
    assert_eq!(
        back["efforts"],
        serde_json::json!(["low", "medium", "high", "xhigh", "max"])
    );
}

#[test]
fn openrouter_draws_no_chips_and_searches_the_list_by_id_or_name_up_to_twenty() {
    let out = run(r#"
const implement = stage("implement", "claude", "fable");
load();
pick(implement, "openrouter");
const drawn = state(implement);
function type(text) {
  implement.input.value = text;
  implement.input.dispatchEvent({ type: "input", target: implement.input, bubbles: true });
  return state(implement);
}
console.log(JSON.stringify({ drawn, id: type("GPT"), name: type("xai: grok"), many: type("acme"), none: type("zzz"), blank: type("") }));
"#);
    assert_eq!(out["drawn"]["chips"], serde_json::json!([]));
    assert_eq!(out["drawn"]["list"], serde_json::json!([]));
    assert_eq!(out["drawn"]["hint"], "Search 30 OpenRouter models.");
    assert_eq!(out["drawn"]["placeholder"], "Model ID");
    assert_eq!(out["drawn"]["model"], "");
    assert_eq!(out["id"]["results"], serde_json::json!(["openai/gpt-5"]));
    assert_eq!(out["name"]["results"], serde_json::json!(["x-ai/grok-4"]));
    assert_eq!(out["many"]["results"].as_array().unwrap().len(), 20);
    assert_eq!(out["none"]["resultsHidden"], true);
    assert_eq!(out["blank"]["resultsHidden"], true);
}

#[test]
fn clicking_a_result_fills_the_field_and_an_unlisted_id_stays() {
    let out = run(r#"
const implement = stage("implement", "claude", "");
load();
pick(implement, "openrouter");
implement.input.value = "grok";
implement.input.dispatchEvent({ type: "input", target: implement.input, bubbles: true });
const item = implement.results.children[0];
item.dispatchEvent({ type: "click", target: item });
const picked = state(implement);
implement.input.value = "my/own-id:free";
implement.input.dispatchEvent({ type: "input", target: implement.input, bubbles: true });
console.log(JSON.stringify({ picked, typed: state(implement) }));
"#);
    assert_eq!(out["picked"]["model"], "x-ai/grok-4");
    assert_eq!(out["picked"]["resultsHidden"], true);
    assert_eq!(out["typed"]["model"], "my/own-id:free");
    assert_eq!(out["typed"]["results"], serde_json::json!([]));
}

#[test]
fn switching_away_from_openrouter_clears_a_listed_id_hides_results_and_restores_chips() {
    let out = run(r#"
const implement = stage("implement", "claude", "");
load();
pick(implement, "openrouter");
implement.input.value = "x-ai/grok-4";
implement.input.dispatchEvent({ type: "input", target: implement.input, bubbles: true });
pick(implement, "claude");
const back = state(implement);
pick(implement, "openrouter");
console.log(JSON.stringify({ back, again: state(implement) }));
"#);
    assert_eq!(out["back"]["model"], "");
    assert_eq!(
        out["back"]["chips"],
        serde_json::json!(["", "fable", "opus"])
    );
    assert_eq!(out["back"]["resultsHidden"], true);
    assert_eq!(out["again"]["chips"], serde_json::json!([]));
}
