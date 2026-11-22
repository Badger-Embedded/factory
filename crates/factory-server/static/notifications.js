(function () {
  if (!("Notification" in window)) return;
  var STORE = "factory.notified";
  var LOCK = "factory-notifications";
  var pending = false;

  var TAB = STORE + "." + Math.random().toString(36).slice(2);

  function entries() {
    var found = [];
    for (var i = 0; i < localStorage.length; i++) {
      var name = localStorage.key(i);
      if (name.indexOf(STORE + ".") !== 0) continue;
      try {
        var state = JSON.parse(localStorage.getItem(name));
        if (state && Array.isArray(state.keys) && Array.isArray(state.snapshot)) {
          found.push({ name: name, state: state });
        }
      } catch (e) {}
    }
    return found;
  }

  function newer(a, b) {
    return a[0] > b[0] || (a[0] === b[0] && a[1] > b[1]);
  }

  function show(item) {
    var verb = item.kind === "failed" ? "failed" : "needs you";
    var note = new Notification("Job #" + item.job + " " + verb, {
      body: item.title,
      tag: item.key,
    });
    note.onclick = function () {
      window.focus();
      window.open(item.url);
      note.close();
    };
  }

  function update(data) {
    var found = entries();
    var best = null;
    found.forEach(function (entry) {
      if (!best || newer(entry.state.snapshot, best.snapshot)) best = entry.state;
    });
    if (best && !newer(data.snapshot, best.snapshot)) return;
    data.items.forEach(function (item) {
      if (best && best.keys.indexOf(item.key) === -1) show(item);
    });
    try {
      localStorage.setItem(
        TAB + "." + data.snapshot.join("-"),
        JSON.stringify({
          snapshot: data.snapshot,
          keys: data.items.map(function (item) {
            return item.key;
          }),
        })
      );
      found.forEach(function (entry) {
        if (newer(data.snapshot, entry.state.snapshot)) {
          localStorage.removeItem(entry.name);
        }
      });
    } catch (e) {}
  }

  function fetchAndUpdate() {
    return fetch("/notifications", { headers: { Accept: "application/json" } })
      .then(function (response) {
        return response.ok ? response.json() : null;
      })
      .then(function (data) {
        if (data && data.enabled && Array.isArray(data.snapshot)) update(data);
      })
      .catch(function () {});
  }

  function poll() {
    if (pending || Notification.permission !== "granted") return;
    pending = true;
    var done = function () {
      pending = false;
    };
    var run = navigator.locks
      ? navigator.locks.request(LOCK, fetchAndUpdate)
      : fetchAndUpdate();
    run.then(done, done);
  }

  poll();
  setInterval(poll, 5000);
})();
