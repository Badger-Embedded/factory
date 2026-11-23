(function () {
  // The job page polls a fragment and htmx swaps each live region in out of
  // band. Two things have to survive a swap: where the reader has scrolled the
  // log, and a form field they are typing in.
  var key = "factory.log-scroll:" + location.pathname;
  function logEl() {
    return document.querySelector("#job-log ol.log");
  }
  // A collapsed Log card hides the list, and a hidden list reports zero for
  // `scrollTop` and `scrollHeight`, so the remembered place is kept as is.
  function hidden(el) {
    var body = el.closest(".card-body");
    return !!body && body.hidden;
  }
  function atBottom(el) {
    return el.scrollHeight - el.scrollTop - el.clientHeight < 4;
  }
  var pinned = true;
  var top = 0;
  function place(el) {
    el.scrollTop = pinned ? el.scrollHeight : top;
  }

  // On load, pick up where the last visit left off, or pin to the bottom.
  var first = logEl();
  if (first) {
    var saved = null;
    try {
      saved = JSON.parse(sessionStorage.getItem(key));
    } catch (e) {}
    if (saved && !saved.bottom) {
      pinned = false;
      top = saved.top;
    }
    place(first);
  }

  // The tool-call card's open groups are kept across a swap by
  // `details-state.js`, which handles every keyed disclosure on the page.

  function typingIn(el) {
    var active = document.activeElement;
    return el.contains(active) && /^(INPUT|TEXTAREA|SELECT)$/.test(active.tagName);
  }

  document.body.addEventListener("htmx:oobBeforeSwap", function (e) {
    var target = e.detail.target;
    if (!target) return;
    // Leave a region the reader is typing in alone, so the text and the focus
    // stay. The next poll swaps it once they move on.
    if (typingIn(target)) {
      e.preventDefault();
      return;
    }
    // Remember the scroll place before the log is replaced.
    if (target.id === "job-log") {
      var el = logEl();
      if (el && !hidden(el)) {
        pinned = atBottom(el);
        top = el.scrollTop;
      }
    }
  });

  document.body.addEventListener("htmx:oobAfterSwap", function (e) {
    if (!e.detail.target) return;
    if (e.detail.target.id === "job-log") {
      var el = logEl();
      if (el) place(el);
    }
  });

  // `job-sections.js` announces a card collapsing, before it hides the card,
  // and expanding, after it shows the card again.
  document.body.addEventListener("factory:section-toggle", function (e) {
    if (e.detail.section !== "log") return;
    var el = logEl();
    if (!el) return;
    if (e.detail.open) {
      place(el);
    } else if (!hidden(el)) {
      pinned = atBottom(el);
      top = el.scrollTop;
    }
  });

  // Save the place for a manual reload or a move to another page.
  window.addEventListener("pagehide", function () {
    var el = logEl();
    var state = el && !hidden(el) ? { bottom: atBottom(el), top: el.scrollTop } : { bottom: pinned, top: top };
    try {
      sessionStorage.setItem(key, JSON.stringify(state));
    } catch (e) {}
  });
})();
