(function () {
  const picker = document.querySelector(".pr-picker");
  if (!picker) return;
  const form = picker.closest("form");
  const search = picker.querySelector("#pr-search");
  const list = picker.querySelector("#pr-results");
  const picked = picker.querySelector(".pr-picked");
  const request = form.querySelector(".request-button");
  const actions = form.querySelector(".review-actions");
  const toggle = actions.querySelector(".split-toggle");
  const menu = actions.querySelector(".split-menu");
  const fixItem = menu.querySelector(".split-item");
  const url = "/projects/" + encodeURIComponent(picker.dataset.project) + "/pulls";
  const DEBOUNCE_MS = 250;

  const chosen = document.createElement("input");
  chosen.type = "hidden";
  chosen.name = "pr";
  picker.appendChild(chosen);
  search.removeAttribute("name");
  search.inputMode = "text";
  search.placeholder = "Search by title or #number";
  search.setAttribute("role", "combobox");
  search.setAttribute("aria-autocomplete", "list");
  search.setAttribute("aria-controls", list.id);
  search.setAttribute("aria-expanded", "false");

  let timer = null;
  let pending = null;
  let loaded = false;

  function rows() {
    return Array.from(list.querySelectorAll("li[data-number]"));
  }

  function activeRow() {
    return list.querySelector("li.active");
  }

  function highlight(row) {
    rows().forEach(function (other) {
      const on = other === row;
      other.classList.toggle("active", on);
      other.setAttribute("aria-selected", on ? "true" : "false");
    });
    if (row) {
      search.setAttribute("aria-activedescendant", row.id);
      row.scrollIntoView({ block: "nearest" });
    } else {
      search.removeAttribute("aria-activedescendant");
    }
  }

  function open() {
    list.hidden = false;
    search.setAttribute("aria-expanded", "true");
  }

  function close() {
    list.hidden = true;
    search.setAttribute("aria-expanded", "false");
    search.removeAttribute("aria-activedescendant");
  }

  function cancel() {
    clearTimeout(timer);
    timer = null;
    if (pending) pending.abort();
    pending = null;
  }

  function load() {
    cancel();
    const controller = new AbortController();
    pending = controller;
    fetch(url + "?q=" + encodeURIComponent(search.value.trim()), { signal: controller.signal })
      .then(function (response) {
        if (!response.ok) throw new Error("the factory answered " + response.status);
        return response.text();
      })
      .then(function (html) {
        if (pending !== controller) return;
        list.innerHTML = html;
        loaded = true;
        open();
        highlight(rows()[0] || null);
      })
      .catch(function (error) {
        if (error.name === "AbortError" || pending !== controller) return;
        const note = document.createElement("li");
        note.className = "pr-note pr-error";
        note.setAttribute("role", "presentation");
        note.textContent = "Could not search pull requests: " + error.message;
        list.replaceChildren(note);
        open();
        highlight(null);
      })
      .finally(function () {
        if (pending === controller) pending = null;
      });
  }

  function openMenu() {
    menu.hidden = false;
    toggle.setAttribute("aria-expanded", "true");
  }

  function closeMenu() {
    menu.hidden = true;
    toggle.setAttribute("aria-expanded", "false");
  }

  function items() {
    return Array.from(menu.querySelectorAll("button:not(:disabled)"));
  }

  toggle.addEventListener("click", function () {
    if (menu.hidden) {
      openMenu();
      const first = items()[0];
      if (first) first.focus();
    } else {
      closeMenu();
    }
  });
  toggle.addEventListener("keydown", function (event) {
    if (event.key === "ArrowDown" && menu.hidden) {
      event.preventDefault();
      openMenu();
      const first = items()[0];
      if (first) first.focus();
    } else if (event.key === "Escape" && !menu.hidden) {
      event.preventDefault();
      closeMenu();
    }
  });
  menu.addEventListener("keydown", function (event) {
    const all = items();
    const at = all.indexOf(document.activeElement);
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      if (all.length === 0) return;
      const step = event.key === "ArrowDown" ? 1 : -1;
      all[(at + step + all.length) % all.length].focus();
    } else if (event.key === "Escape") {
      event.preventDefault();
      closeMenu();
      toggle.focus();
    }
  });
  document.addEventListener("click", function (event) {
    if (!actions.contains(event.target)) closeMenu();
  });

  function pick(row) {
    cancel();
    chosen.value = row.dataset.number;
    picked.replaceChildren(row.querySelector("template").content.cloneNode(true));
    const remove = document.createElement("button");
    remove.type = "button";
    remove.className = "pr-remove";
    remove.textContent = "Remove";
    remove.addEventListener("click", clear);
    picked.insertBefore(remove, picked.querySelector(".pr-hint"));
    picked.hidden = false;
    picked.dataset.number = row.dataset.number;
    const blocked = row.dataset.fork === "true" || row.dataset.heldJob !== undefined;
    request.disabled = blocked;
    fixItem.disabled = blocked;
    actions.hidden = false;
    search.hidden = true;
    picker.querySelector(".pr-label").hidden = true;
    close();
    // Hiding the focused box would drop focus from the form, which shrinks
    // the text box under the pointer.
    form.querySelector("textarea").focus();
  }

  function clear() {
    chosen.value = "";
    request.disabled = false;
    fixItem.disabled = false;
    actions.hidden = true;
    closeMenu();
    actions.querySelector("input[name=comment]").checked = false;
    search.hidden = false;
    picker.querySelector(".pr-label").hidden = false;
    search.value = "";
    loaded = false;
    // Focus first: removing the focused Remove button drops focus from the
    // form, which hides the rest of the form before the box can take it.
    search.focus();
    picked.hidden = true;
    picked.replaceChildren();
  }

  search.addEventListener("input", function () {
    cancel();
    loaded = false;
    list.replaceChildren();
    highlight(null);
    close();
    timer = setTimeout(load, DEBOUNCE_MS);
  });
  search.addEventListener("focus", function () {
    if (!loaded) load();
    else open();
  });
  search.addEventListener("keydown", function (event) {
    if (event.isComposing || event.keyCode === 229) return;
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      if (list.hidden) {
        if (loaded) open();
        else load();
        return;
      }
      const all = rows();
      if (all.length === 0) return;
      const at = all.indexOf(activeRow());
      const step = event.key === "ArrowDown" ? 1 : -1;
      highlight(all[Math.min(Math.max(at + step, 0), all.length - 1)]);
    } else if (event.key === "Enter") {
      event.preventDefault();
      const row = activeRow();
      if (row && !list.hidden) pick(row);
    } else if (event.key === "Escape" && (!list.hidden || pending || timer)) {
      event.preventDefault();
      cancel();
      close();
    }
  });
  list.addEventListener("mousedown", function (event) {
    const row = event.target.closest("li[data-number]");
    if (!row) return;
    event.preventDefault();
    pick(row);
  });
  list.addEventListener("mousemove", function (event) {
    const row = event.target.closest("li[data-number]");
    if (row && row !== activeRow()) highlight(row);
  });
  document.addEventListener("click", function (event) {
    if (!picker.contains(event.target)) close();
  });
})();
