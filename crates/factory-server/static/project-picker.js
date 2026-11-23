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
  search.hidden = false;

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
      search.setAttribute("aria-activedescendant", row.id);
      row.scrollIntoView({ block: "nearest" });
    } else {
      search.removeAttribute("aria-activedescendant");
    }
  }

  // Hides the rows that miss the query and keeps the highlighted project when
  // it is still shown, else highlights the first shown row.
  function applyFilter() {
    const query = search.value.trim().toLowerCase();
    rows().forEach(function (row) {
      row.hidden = query !== "" &&
        !row.dataset.name.toLowerCase().includes(query) &&
        !row.dataset.id.toLowerCase().includes(query);
    });
    const shown = visibleRows();
    empty.hidden = shown.length > 0;
    highlight(shown.find(function (row) { return row.dataset.id === activeId; }) || shown[0] || null);
  }

  search.addEventListener("input", applyFilter);
  search.addEventListener("keydown", function (event) {
    const shown = visibleRows();
    const at = shown.findIndex(function (row) { return row.classList.contains("active"); });
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      if (shown.length === 0) return;
      const step = event.key === "ArrowDown" ? 1 : -1;
      highlight(shown[Math.min(Math.max(at + step, 0), shown.length - 1)]);
    } else if (event.key === "Enter") {
      event.preventDefault();
      const link = at >= 0 ? shown[at].querySelector("a") : null;
      if (link) window.location.href = link.href;
    }
  });

  picker.addEventListener("toggle", function () {
    summary.setAttribute("aria-expanded", picker.open ? "true" : "false");
    if (picker.open) {
      applyFilter();
      search.focus();
    } else {
      search.value = "";
      applyFilter();
    }
    fitPanel();
  });
  window.addEventListener("resize", fitPanel);
  document.addEventListener("click", function (event) {
    if (picker.open && !picker.contains(event.target)) picker.open = false;
  });
  document.addEventListener("keydown", function (event) {
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
    applyFilter();
    if (focusedHref !== null) {
      for (const link of list.querySelectorAll("a")) {
        if (link.getAttribute("href") === focusedHref) link.focus();
      }
      focusedHref = null;
    }
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
        applyFilter();
      });
  });
})();
