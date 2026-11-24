(function () {
  const picker = document.getElementById("project-picker");
  const summary = picker.querySelector("summary");
  const panel = picker.querySelector(".picker-panel");
  const current = picker.querySelector(".picker-current");

  function fitPanel() {
    panel.style.maxHeight = "";
    if (!picker.open) return;
    const room = window.innerHeight - panel.getBoundingClientRect().top - 8;
    panel.style.maxHeight = Math.max(room, 0) + "px";
  }

  const search = document.getElementById("picker-search");
  const empty = picker.querySelector(".picker-empty");
  const status = picker.querySelector(".picker-status");
  search.hidden = false;
  const keys = picker.querySelector(".picker-keys");
  const keyCap = summary.querySelector(".picker-key");
  keys.hidden = false;
  keyCap.hidden = false;

  function rows() {
    const list = document.getElementById("picker-list");
    return list ? Array.from(list.querySelectorAll("li")) : [];
  }

  function visibleRows() {
    return rows().filter(function (row) { return !row.hidden; });
  }

  let activeId = null;

  function highlight(row) {
    activeId = row ? row.dataset.id : null;
    rows().forEach(function (other) { other.classList.toggle("active", other === row); });
    if (row) {
      status.textContent = row.dataset.name;
      row.scrollIntoView({ block: "nearest" });
    } else {
      status.textContent = search.value.trim() === "" ? "" : "No project matches.";
    }
  }

  function applyFilter(keepActive) {
    const query = search.value.trim().toLowerCase();
    rows().forEach(function (row) {
      row.hidden = query !== "" &&
        !row.dataset.name.toLowerCase().includes(query) &&
        !row.dataset.id.toLowerCase().includes(query);
    });
    const shown = visibleRows();
    empty.hidden = shown.length > 0;
    const kept = keepActive === true && shown.find(function (row) { return row.dataset.id === activeId; });
    highlight(kept || shown[0] || null);
  }

  function needsYou(row) {
    return row.dataset.attention === "you" || row.dataset.attention === "failed";
  }

  function otherAttention() {
    return rows().filter(function (row) {
      return needsYou(row) && !row.querySelector('a[aria-current="page"]');
    });
  }

  function dimHints() {
    const none = otherAttention().length === 0;
    keys.querySelectorAll(".picker-hint-n").forEach(function (hint) {
      hint.classList.toggle("dimmed", none);
    });
  }

  function jump(step) {
    const all = rows();
    const at = all.findIndex(function (row) { return row.querySelector('a[aria-current="page"]'); });
    for (let i = 1; i <= all.length; i++) {
      const row = all[(((at < 0 ? (step > 0 ? -1 : 0) : at) + step * i) % all.length + all.length) % all.length];
      if (row.querySelector('a[aria-current="page"]')) continue;
      const link = row.querySelector("a");
      if (needsYou(row) && link) {
        window.location.assign(link.href);
        return;
      }
    }
  }

  function moveHighlight(step) {
    const shown = visibleRows();
    if (shown.length === 0) return;
    const at = shown.findIndex(function (row) { return row.classList.contains("active"); });
    highlight(shown[Math.min(Math.max(at + step, 0), shown.length - 1)]);
  }

  function openHighlighted() {
    const row = visibleRows().find(function (other) { return other.classList.contains("active"); });
    const link = row ? row.querySelector("a") : null;
    if (link) window.location.href = link.href;
  }

  search.addEventListener("input", function () { applyFilter(false); });
  search.addEventListener("keydown", function (event) {
    if (event.isComposing || event.keyCode === 229) return;
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      moveHighlight(event.key === "ArrowDown" ? 1 : -1);
    } else if (event.key === "Enter") {
      event.preventDefault();
      openHighlighted();
    }
  });

  let normalMode = false;

  picker.addEventListener("toggle", function () {
    summary.setAttribute("aria-expanded", picker.open ? "true" : "false");
    if (picker.open) {
      applyFilter(true);
      if (!normalMode) search.focus();
      normalMode = false;
    } else {
      normalMode = false;
      search.value = "";
      applyFilter(false);
    }
    fitPanel();
  });
  window.addEventListener("resize", fitPanel);
  document.addEventListener("click", function (event) {
    if (picker.open && !picker.contains(event.target)) picker.open = false;
  });
  document.addEventListener("keydown", function (event) {
    if (event.isComposing || event.keyCode === 229) return;
    if (event.ctrlKey || event.altKey || event.metaKey) return;
    const target = event.target;
    const typing = target instanceof Element &&
      (target.closest("input, textarea, select, [contenteditable]") || target.isContentEditable);
    if (!typing) {
      const key = event.key.toLowerCase();
      if (key === "p" && !event.shiftKey) {
        event.preventDefault();
        if (!picker.open) {
          normalMode = true;
          picker.open = true;
          applyFilter(true);
        }
        return;
      }
      if (key === "n") {
        event.preventDefault();
        jump(event.shiftKey ? -1 : 1);
        return;
      }
      if (picker.open) {
        const plain = !event.shiftKey;
        if ((key === "j" && plain) || event.key === "ArrowDown") {
          event.preventDefault();
          moveHighlight(1);
          return;
        }
        if ((key === "k" && plain) || event.key === "ArrowUp") {
          event.preventDefault();
          moveHighlight(-1);
          return;
        }
        if (key === "i" && plain) {
          event.preventDefault();
          search.focus();
          return;
        }
        if (event.key === "Enter" && !(target instanceof Element && target.closest("#picker-list a"))) {
          event.preventDefault();
          openHighlighted();
          return;
        }
      }
    }
    if (event.key === "Escape" && picker.open) {
      picker.open = false;
      summary.focus();
    }
  });

  let focusedHref = null;
  let detached = false;

  function clearCurrent(list) {
    list.querySelectorAll("[aria-current]").forEach(function (link) {
      link.removeAttribute("aria-current");
    });
  }

  document.body.addEventListener("htmx:beforeSwap", function (event) {
    const target = event.detail.target;
    if (!target || target.id !== "picker-list") return;
    if (detached && event.detail.xhr.responseText.trim() === "") {
      event.detail.shouldSwap = false;
      return;
    }
    const active = document.activeElement;
    focusedHref = active && target.contains(active) ? active.getAttribute("href") : null;
  });
  document.body.addEventListener("htmx:afterSwap", function (event) {
    const target = event.detail.target;
    if (!target || target.id !== "picker-list") return;
    const list = document.getElementById("picker-list");
    if (detached) clearCurrent(list);
    const row = list.querySelector('a[aria-current="page"] .picker-name');
    if (row) current.textContent = row.textContent;
    applyFilter(true);
    dimHints();
    if (focusedHref !== null) {
      for (const link of list.querySelectorAll("a")) {
        if (link.getAttribute("href") === focusedHref) link.focus();
      }
      focusedHref = null;
    }
  });
  document.body.addEventListener("htmx:afterSettle", function (event) {
    const target = event.detail.target;
    if (!target || target.id !== "picker-list") return;
    applyFilter(true);
  });
  document.body.addEventListener("htmx:responseError", function (event) {
    const target = event.detail.target;
    if (!target || target.id !== "picker-list" || event.detail.xhr.status !== 404) return;
    fetch("/fragments/picker", { headers: { Accept: "text/html" } })
      .then(function (response) { return response.text(); })
      .then(function (html) {
        const list = document.getElementById("picker-list");
        const badge = document.getElementById("picker-badge");
        if (!list) return;
        const doc = new DOMParser().parseFromString(html, "text/html");
        const fresh = doc.getElementById("picker-list");
        const freshBadge = doc.getElementById("picker-badge");
        if (!fresh) return;
        detached = true;
        clearCurrent(fresh);
        list.replaceWith(fresh);
        if (badge && freshBadge) {
          freshBadge.removeAttribute("hx-swap-oob");
          badge.replaceWith(freshBadge);
        }
        htmx.process(fresh);
        applyFilter(true);
        dimHints();
      });
  });
  dimHints();
})();
