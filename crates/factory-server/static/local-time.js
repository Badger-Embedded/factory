// Rewrites each <time> the server renders from its UTC value to the browser's
// own time zone. The server writes the UTC time in the `datetime` attribute
// (ISO 8601 with a Z) and the same UTC text inside the element, so a page with
// this script off still shows a correct time. `data-format` picks the layout:
// `datetime` is a full date and time with the zone name, `time` is HH:MM, and
// `time-seconds` is HH:MM:SS. The title always carries the full local time.
//
// It runs on load, and on htmx's `afterSwap` and `oobAfterSwap` events, which
// htmx fires in the same task as the swap, before the browser paints. So the
// 5-second fragment polls and out-of-band swaps show local time with no UTC
// frame in between. It also runs on `htmx:load`, which htmx fires 20 ms later
// in the settle step, as a backstop for content added by other means. It reads
// only the `datetime` attribute, never the current text, so running it twice
// on one element changes nothing.
(function () {
  function parts(date) {
    var out = {};
    new Intl.DateTimeFormat("en-US", {
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
      hour12: false,
    })
      .formatToParts(date)
      .forEach(function (part) {
        out[part.type] = part.value;
      });
    // hour12:false reports midnight as 24 on some engines.
    if (out.hour === "24") out.hour = "00";
    return out;
  }

  function zone(date) {
    try {
      var found = new Intl.DateTimeFormat("en-US", { timeZoneName: "short" })
        .formatToParts(date)
        .find(function (part) {
          return part.type === "timeZoneName";
        });
      if (found) return found.value;
    } catch (error) {
      // Fall through to UTC.
    }
    return "UTC";
  }

  function convert(element) {
    var iso = element.getAttribute("datetime");
    if (!iso) return;
    var date = new Date(iso);
    var time = date.getTime();
    // An invalid date's time is the one value not equal to itself.
    if (time !== time) return;
    var p = parts(date);
    var day = p.year + "-" + p.month + "-" + p.day;
    var hm = p.hour + ":" + p.minute;
    var hms = hm + ":" + p.second;
    var name = zone(date);
    var format = element.getAttribute("data-format");
    var text;
    if (format === "time") {
      text = hm;
    } else if (format === "time-seconds") {
      text = hms;
    } else {
      text = day + " " + hms + " " + name;
    }
    element.textContent = text;
    element.title = day + " " + hms + " " + name;
  }

  function run(root) {
    var nodes = (root || document).querySelectorAll("time[datetime][data-format]");
    for (var i = 0; i < nodes.length; i++) convert(nodes[i]);
  }

  run(document);
  // htmx sets `event.detail.target` to the element it swapped content into, for
  // both the main swap (`htmx:afterSwap`) and each out-of-band swap
  // (`htmx:oobAfterSwap`). Both fire in the same task as the swap, so the
  // conversion lands before the browser paints and no UTC frame shows. Listen
  // on document.body, which works even though htmx, a deferred script, loads
  // after this inline one runs.
  function onSwap(event) {
    run((event.detail && event.detail.target) || event.target || document);
  }
  document.body.addEventListener("htmx:afterSwap", onSwap);
  document.body.addEventListener("htmx:oobAfterSwap", onSwap);
  // Backstop: htmx:load fires in the settle step, 20 ms later, for content
  // added outside a swap.
  document.body.addEventListener("htmx:load", function (event) {
    run(event.detail && event.detail.elt);
  });
})();
