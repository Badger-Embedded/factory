// Keeps an expanded disclosure open across an htmx swap. Every 5 seconds a
// region of the board, or an out-of-band region of the job page, is replaced
// with freshly rendered markup, and the server renders each disclosure closed.
// Without this a reader looking at "Show N older" on the board, or at an open
// MCP tool-call group on the job page, would see it fold up within one poll.
//
// Each disclosure the reader can open carries a stable `data-key` that survives
// the re-render: the section name for `details.older`, the server name for an
// MCP group. This script runs once per page, outside every swapped region. It
// records whether each keyed disclosure is open just before a swap, on
// `htmx:beforeSwap` and `htmx:oobBeforeSwap`, and sets the open ones open again
// just after, on `htmx:afterSwap` and `htmx:oobAfterSwap`. htmx fires the after
// events in the same task as the swap, so a reopened section never paints
// closed. A reader who closes a section records it closed, so the next poll
// keeps it closed, and a key whose element is gone reopens nothing. The state
// lasts for the page's lifetime and is not kept across a reload.
(function () {
  // key -> true when open, false when the reader closed it.
  var state = {};
  // Keys inside the content being swapped. htmx reports the replaced element
  // as the target after an outerHTML swap, and it has left the page by then.
  var swapped = [];

  function remember(root) {
    if (!root || !root.querySelectorAll) return;
    swapped = [];
    root.querySelectorAll("details[data-key]").forEach(function (d) {
      var key = d.getAttribute("data-key");
      state[key] = d.open;
      swapped.push(key);
    });
  }

  function restore() {
    document.querySelectorAll("details[data-key]").forEach(function (d) {
      var key = d.getAttribute("data-key");
      if (swapped.indexOf(key) !== -1 && state[key]) d.open = true;
    });
  }

  function before(event) {
    remember(event.detail && event.detail.target);
  }
  function after() {
    restore();
  }

  document.body.addEventListener("htmx:beforeSwap", before);
  document.body.addEventListener("htmx:oobBeforeSwap", before);
  document.body.addEventListener("htmx:afterSwap", after);
  document.body.addEventListener("htmx:oobAfterSwap", after);
})();
