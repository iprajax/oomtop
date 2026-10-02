/* oomtop site: page interactions (no framework, no build step). The 3D scene lives in city.js. */
(function () {
  "use strict";

  var GiB = 1073741824, MiB = 1048576;
  // Same units as oomtop's default format.memory_units (IEC, one decimal).
  function fmt(b) {
    if (b == null || isNaN(b)) return "n/a";
    if (Math.abs(b) >= GiB) return (b / GiB).toFixed(1) + " GiB";
    return (b / MiB).toFixed(1) + " MiB";
  }
  window.oomtopFmt = fmt;

  var snapshot = window.oomtopSnapshot = fetch("data/snapshot.json").then(function (r) {
    if (!r.ok) throw new Error("snapshot " + r.status);
    return r.json();
  });

  /* ── theme toggle (manual override; default follows prefers-color-scheme) ── */
  var root = document.documentElement;
  var toggle = document.getElementById("theme-toggle");
  function currentTheme() {
    if (root.dataset.theme) return root.dataset.theme;
    return window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
  }
  function labelToggle() {
    toggle.setAttribute("aria-label", currentTheme() === "dark" ? "Switch to light theme" : "Switch to dark theme");
  }
  if (toggle) {
    labelToggle();
    toggle.addEventListener("click", function () {
      var next = currentTheme() === "dark" ? "light" : "dark";
      root.dataset.theme = next;
      try { localStorage.setItem("oomtop-theme", next); } catch (e) {}
      labelToggle();
      window.dispatchEvent(new CustomEvent("oomtop-theme"));
    });
    window.matchMedia("(prefers-color-scheme: light)").addEventListener("change", function () {
      labelToggle();
      window.dispatchEvent(new CustomEvent("oomtop-theme"));
    });
  }

  /* ── tabs (WAI-ARIA tabs pattern) ── */
  document.querySelectorAll("[data-tabs]").forEach(function (box) {
    var tabs = Array.prototype.slice.call(box.querySelectorAll('[role="tab"]'));
    function select(tab, focus) {
      tabs.forEach(function (t) {
        var on = t === tab;
        t.setAttribute("aria-selected", on ? "true" : "false");
        t.tabIndex = on ? 0 : -1;
        var p = document.getElementById(t.getAttribute("aria-controls"));
        if (p) p.hidden = !on;
      });
      if (focus) tab.focus();
      var note = document.getElementById("cta-note");
      if (note && box.contains(note)) {
        note.textContent = tab.id === "cta-tab-brew"
          ? "Homebrew goes live with the first tagged release. Today: build from source (Rust 1.88+)."
          : "Works today on macOS and Linux with Rust 1.88+. Binary at target/release/oomtop.";
      }
    }
    tabs.forEach(function (t, i) {
      t.addEventListener("click", function () { select(t, false); });
      t.addEventListener("keydown", function (e) {
        var j = null;
        if (e.key === "ArrowRight") j = (i + 1) % tabs.length;
        else if (e.key === "ArrowLeft") j = (i - 1 + tabs.length) % tabs.length;
        else if (e.key === "Home") j = 0;
        else if (e.key === "End") j = tabs.length - 1;
        if (j !== null) { e.preventDefault(); select(tabs[j], true); }
      });
    });
  });

  /* ── copy buttons ── */
  function copyText(text) {
    if (navigator.clipboard && window.isSecureContext) return navigator.clipboard.writeText(text);
    return new Promise(function (res, rej) {
      var ta = document.createElement("textarea");
      ta.value = text; ta.setAttribute("readonly", ""); ta.style.position = "fixed"; ta.style.opacity = "0";
      document.body.appendChild(ta); ta.select();
      try { document.execCommand("copy") ? res() : rej(); } catch (e) { rej(e); }
      document.body.removeChild(ta);
    });
  }
  document.addEventListener("click", function (e) {
    var b = e.target.closest && e.target.closest("[data-copy]");
    if (!b) return;
    copyText(b.getAttribute("data-copy")).then(function () {
      b.textContent = "Copied"; b.classList.add("done");
      setTimeout(function () { b.textContent = "Copy"; b.classList.remove("done"); }, 1600);
    }, function () { b.textContent = "Select"; });
  });

  /* ── data-driven bits ── */
  snapshot.then(function (d) {
    var vals = {
      total: d.memory.total, swap_used: d.memory.swap_used, swap_total: d.memory.swap_total, headroom: d.headroom.bytes
    };
    document.querySelectorAll("[data-fmt]").forEach(function (el) {
      var k = el.getAttribute("data-fmt");
      if (k in vals) el.textContent = k === "total" ? Math.round(vals[k] / GiB) + " GiB" : fmt(vals[k]);
    });
    memBar(d);
    calc(d);
  }).catch(function (err) {
    console.warn("oomtop: snapshot unavailable", err);
  });

  /* Mem[…] bar, with the TUI's glyphs: apps | · compressed * · wired = · cache . */
  function memBar(d) {
    var bar = document.getElementById("membar-bar");
    if (!bar) return;
    var m = d.memory, total = m.total;
    var parts = [
      { k: "app", g: "|", cls: "g-app", label: "apps", v: m.app },
      { k: "compressed", g: "*", cls: "g-comp", label: "compressed", v: m.compressed },
      { k: "wired", g: "=", cls: "g-wired", label: "wired", v: m.wired },
      { k: "cached", g: ".", cls: "g-cache", label: "cache", v: m.cached }
    ];
    var used = parts.reduce(function (s, p) { return s + p.v; }, 0);
    var free = Math.max(0, total - used);
    parts.push({ k: "free", g: " ", cls: "g-free", label: "free / other", v: free });
    function draw() {
      // fit the glyph count to the available width
      var probe = document.createElement("span"); probe.textContent = "||||||||||"; probe.style.visibility = "hidden";
      bar.textContent = ""; bar.appendChild(probe);
      var cw = probe.getBoundingClientRect().width / 10 || 8;
      var n = Math.max(20, Math.floor(bar.getBoundingClientRect().width / cw) - 1);
      bar.textContent = "";
      var acc = 0, drawn = 0;
      parts.forEach(function (p) {
        acc += p.v;
        var upto = Math.round(acc / total * n);
        var cnt = Math.max(0, upto - drawn); drawn = upto;
        var s = document.createElement("span"); s.className = p.cls;
        s.textContent = new Array(cnt + 1).join(p.g);
        bar.appendChild(s);
      });
    }
    draw();
    var t; window.addEventListener("resize", function () { clearTimeout(t); t = setTimeout(draw, 120); });
    document.getElementById("membar-total").textContent = fmt(total - m.available) + " used of " + Math.round(total / GiB) + " GiB";
    var lg = document.getElementById("membar-legend");
    parts.forEach(function (p) {
      var li = document.createElement("li");
      li.innerHTML = '<span class="glyph ' + p.cls + '">' + (p.g === " " ? "&nbsp;" : p.g) + '</span><span>' + p.label + '</span><span class="v">' + fmt(p.v) + "</span>";
      lg.appendChild(li);
    });
    var li2 = document.createElement("li");
    li2.innerHTML = '<span class="glyph">~</span><span>swap</span><span class="v">' + fmt(m.swap_used) + " / " + fmt(m.swap_total) + "</span>";
    lg.appendChild(li2);
  }

  /* ── headroom calculator: same rule as `oomtop headroom` (greedy reclaim, largest gain first) ── */
  function headroomAnswer(d, need) {
    var h = d.headroom.bytes;
    if (need <= h) return { answer: "yes", exit: 0, reclaim: [], gain: 0, shortfall: 0 };
    var plan = [], gain = 0;
    var cands = d.headroom.reclaim.slice().sort(function (a, b) { return b.gain - a.gain; });
    for (var i = 0; i < cands.length; i++) {
      plan.push(cands[i]); gain += cands[i].gain;
      if (h + gain >= need) return { answer: "yes_after_reclaim", exit: 3, reclaim: plan, gain: gain, shortfall: 0 };
    }
    return { answer: "no", exit: 4, reclaim: [], gain: gain, shortfall: need - h - gain };
  }
  window.oomtopHeadroom = headroomAnswer;

  function listJoin(a) {
    if (a.length <= 1) return a.join("");
    return a.slice(0, -1).join(", ") + " and " + a[a.length - 1];
  }
  function sentence(d, need, r) {
    var n = fmt(need);
    if (r.answer === "yes") return "Yes: " + n + " fits — " + fmt(d.headroom.bytes) + " headroom.";
    if (r.answer === "yes_after_reclaim") {
      var names = r.reclaim.map(function (c) { return c.label + " (≈" + fmt(c.gain) + ")"; });
      return "Yes, after reclaim: stopping " + listJoin(names) + " frees ≈" + fmt(r.gain) + ", then " + n + " fits.";
    }
    return "No: " + n + " doesn't fit — short by " + fmt(r.shortfall) + " even after reclaiming ≈" + fmt(r.gain) + ".";
  }
  window.oomtopSentence = sentence;

  function calc(d) {
    var input = document.getElementById("need");
    if (!input) return;
    var out = document.getElementById("need-out"), cmd = document.getElementById("calc-cmd"),
        res = document.getElementById("calc-out"), verdict = document.getElementById("calc-verdict"),
        exit = document.getElementById("calc-exit");
    var segH = document.getElementById("cb-head"), segs = [document.getElementById("cb-r1"), document.getElementById("cb-r2")],
        needMark = document.getElementById("cb-need");
    var max = parseFloat(input.max) * GiB;
    var cands = d.headroom.reclaim.slice().sort(function (a, b) { return b.gain - a.gain; });
    segH.style.width = (d.headroom.bytes / max * 100) + "%";
    segs.forEach(function (s, i) {
      if (cands[i]) { s.style.width = (cands[i].gain / max * 100) + "%"; s.title = cands[i].label + " ≈" + fmt(cands[i].gain); }
      else s.style.display = "none";
    });
    function update() {
      var g = parseFloat(input.value);
      var need = Math.round(g * GiB);
      var r = headroomAnswer(d, need);
      out.textContent = g.toFixed(1) + " GiB";
      cmd.textContent = "oomtop headroom --need " + g.toFixed(1) + "G";
      var lines = sentence(d, need, r);
      res.innerHTML = "";
      res.appendChild(document.createTextNode(lines));
      if (r.answer === "yes_after_reclaim") {
        var run = document.createElement("div"); run.textContent = "Run: oomtop reclaim --groups …"; res.appendChild(run);
      }
      var note = document.createElement("div"); note.className = "dim";
      note.textContent = "note: advisory: another process may take this memory before you load";
      res.appendChild(note);
      verdict.className = "verdict " + (r.answer === "yes" ? "v-yes" : r.answer === "no" ? "v-no" : "v-after");
      verdict.textContent = r.answer === "yes" ? "Yes" : r.answer === "no" ? "No" : "Yes, after reclaim";
      exit.textContent = r.exit;
      input.style.setProperty("--p", ((g - input.min) / (input.max - input.min) * 100) + "%");
      needMark.style.left = "calc(" + Math.min(100, need / max * 100) + "% - 1px)";
      input.setAttribute("aria-valuetext", g.toFixed(1) + " GiB: " + verdict.textContent + ", exit " + r.exit);
    }
    input.addEventListener("input", update);
    update();
  }

  /* ── keyboard map ── */
  var FKEYS = [
    ["F1", "Help", "Help: the key reference (also ?). The bar's labels come from the active keymap, so a remapped key shows its new action.", "Help"],
    ["F2", "Setup", "Setup: the settings screen (also ,). Edits are written to config.toml with your comments preserved.", "Set"],
    ["F3", "Search", "Search: the command palette (also Ctrl-K).", "Find"],
    ["F4", "Filter", "Filter the list: mem>2G kind:daemon idle>30m, gpu hogs, claude.", "Filt"],
    ["F5", "Tree", "Tree: in Processes, a parent/child tree with ├─ / └─ branches; on Home, expand or collapse every group.", "Tree"],
    ["F6", "SortBy", "SortBy: a picker listing this view's sort keys, the current one preselected. ↑↓ + enter applies.", "Sort"],
    ["F7", "Why", "Why: why this row is ranked here, with the evidence (also i).", "Why"],
    ["F8", "Reclaim", "Reclaim: jump to view 5, idle build daemons, orphans and idle model servers, largest gain first.", "Recl"],
    ["F9", "Stop", "Stop the selected group. Always confirms inline, like x. SIGTERM first; SIGKILL only on a second yes.", "Stop"],
    ["F10", "Quit", "Quit. F10 Quit always stays on the bar, even in narrow terminals.", "Quit"]
  ];
  var GROUPS = [
    ["Move", [["↑↓ / jk", "move", "Move the selection."], ["enter", "expand", "Expand a group to its members."], ["g / G", "top / bottom", "Jump to the top or bottom of the list."]]],
    ["Views", [["1", "Home", "Home: the answer-first headline and groups ranked by what matters."], ["2", "Processes", "Processes: every process, htop-style, with F5 tree."], ["3", "Models", "Models: model servers, then the model files on disk."], ["4", "Sandboxes", "Sandboxes: VMs and containers with their host cost."], ["5", "Reclaim", "Reclaim: what's idle and what stopping it would free."], ["6", "Timeline", "Timeline: the last 10 minutes with markers such as “swap +2 GB”."]]],
    ["Find", [["/", "filter", "Filter: mem>2G kind:daemon idle>30m, gpu hogs, claude."], [":", "command", "Command line, e.g. :sort …"], ["Ctrl-K", "palette", "The command palette."]]],
    ["Act (always confirms)", [["x", "stop", "Stop the selected group: the row turns into “Stop GradleDaemon? y / n”."], ["z", "suspend", "Suspend/resume: CPU relief only; frees no memory."]]],
    ["Make it yours", [["p", "pin", "Pin: keep this entity in the “Your things” strip."], ["m", "mute", "Mute: rank rows like this lower (an explicit signal that decays slowly)."], ["n", "rename", "Rename a group."], ["i", "why ranked here", "Why this row is ranked here."], ["w", "watch", "Focus mode: one entity full-screen with its memory over time."], ["c", "compare", "Compare two entities."], [",", "settings", "Settings."]]]
  ];
  var fbar = document.getElementById("fbar"), groupsEl = document.getElementById("key-groups"),
      detail = document.getElementById("key-detail"), keymap = document.getElementById("keymap");
  var allButtons = [];
  function show(btn, key, label, text, group) {
    allButtons.forEach(function (b) { b.setAttribute("aria-pressed", b === btn ? "true" : "false"); });
    detail.innerHTML = "";
    var k = document.createElement("div"); k.className = "kd-key";
    k.textContent = key + " · " + label;
    var sm = document.createElement("small"); sm.textContent = group; k.appendChild(sm);
    var p = document.createElement("p"); p.textContent = text;
    detail.appendChild(k); detail.appendChild(p);
  }
  if (fbar) {
    var byKey = {};
    FKEYS.forEach(function (f) {
      var b = document.createElement("button");
      b.type = "button"; b.className = "fkey"; b.setAttribute("aria-pressed", "false");
      b.innerHTML = '<span class="fk">' + f[0] + '</span><span class="fl"><span class="fl-long">' + f[1] + '</span><span class="fs">' + f[3] + "</span></span>";
      b.addEventListener("click", function () { show(b, f[0], f[1], f[2], "function key"); });
      fbar.appendChild(b); allButtons.push(b);
      byKey[f[0].toLowerCase()] = b;
    });
    GROUPS.forEach(function (g) {
      var box = document.createElement("div"); box.className = "kg";
      var h = document.createElement("h3"); h.textContent = g[0]; box.appendChild(h);
      var keys = document.createElement("div"); keys.className = "kg-keys";
      g[1].forEach(function (k) {
        var b = document.createElement("button");
        b.type = "button"; b.className = "kcap"; b.setAttribute("aria-pressed", "false");
        b.innerHTML = "<b></b><span></span>";
        b.firstChild.textContent = k[0]; b.lastChild.textContent = k[1];
        b.addEventListener("click", function () { show(b, k[0], k[1], k[2], g[0]); });
        keys.appendChild(b); allButtons.push(b);
        k[0].split(" / ").forEach(function (part) {
          if (part === "↑↓") { byKey.arrowup = b; byKey.arrowdown = b; }
          else if (part === "jk") { byKey.j = b; byKey.k = b; }
          else if (part === "g") { byKey.g = b; }
          else if (part === "G") { byKey["shift+g"] = b; }
          else if (part === "Ctrl-K") { byKey["ctrl+k"] = b; }
          else byKey[part.toLowerCase()] = b;
        });
      });
      box.appendChild(keys); groupsEl.appendChild(box);
    });
    var hint = document.createElement("p"); hint.className = "note";
    hint.textContent = "Focus this panel and press a key to look it up. Presets: default, vim, emacs, htop (k also means stop).";
    keymap.appendChild(hint);
    allButtons[0].click();
    keymap.addEventListener("keydown", function (e) {
      var k = e.key.toLowerCase();
      if (e.ctrlKey && k === "k") k = "ctrl+k";
      else if (e.shiftKey && k === "g") k = "shift+g";
      if (k === "enter" && document.activeElement !== keymap) return; // let buttons activate normally
      var b = byKey[k];
      if (!b) return;
      if (/^f\d+$/.test(k) || k === "ctrl+k" || k === "/" || k.indexOf("arrow") === 0) e.preventDefault();
      b.click();
    });
  }
})();
