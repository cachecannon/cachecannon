/* valkey-lab — page interactions + terminal animation */
(function () {
  "use strict";

  /* ---- helpers ---- */
  var C = function (s) { return '<span class="t-cyan">' + s + "</span>"; };
  var D = function (s) { return '<span class="t-dim">' + s + "</span>"; };
  var B = function (s) { return '<span class="t-bold">' + s + "</span>"; };
  var G = function (s) { return '<span class="t-green">' + s + "</span>"; };
  var R = function (s) { return '<span class="t-red">' + s + "</span>"; };
  var Y = function (s) { return '<span class="t-yellow">' + s + "</span>"; };
  var L = function (cls, html) { return '<span class="line' + (cls ? " " + cls : "") + '">' + html + "</span>"; };

  /* header + separator rows (reused across scenarios) */
  var HDR = C("time UTC  req/s      p50      p90      p99     p999    p9999      max  err/s  hit%");
  var SEP = D("────────  ─────  ───────  ───────  ───────  ───────  ───────  ───────  ─────  ────");

  function sampleRow(time, rate, p50, p90, p99, p999, p9999, max, err, hit) {
    return D(time) + "  " + B(rate) + "  " + D(p50) + "  " + D(p90) + "  " +
           D(p99) + "  " + B(p999) + "  " + D(p9999) + "  " + D(max) + "  " +
           D(err) + "  " + D(hit);
  }

  /* ---- Scenario definitions ---- */
  var scenarios = {
    "default": {
      title: "valkey-lab",
      lines: [
        ["", D("$") + " " + C("valkey-lab")],
        ["", ""],
        ["", B(C("valkey-lab, powered by cachecannon"))],
        ["", D("──────────────────")],
        ["", C("target") + "     127.0.0.1:6379 (Resp)"],
        ["", C("workload") + "   1M keys, 16B key, 64B value, 80:20 GET:SET"],
        ["", C("threads") + "    8"],
        ["", C("conns") + "      1, pipeline 1"],
        ["med", ""],
        ["med", "[precheck ok 8ms]"],
        ["med", "[warmup 10s]"],
        ["med", "[running 60s]"],
        ["med", ""],
        ["med", HDR],
        ["med", SEP],
        ["med", sampleRow("14:30:11", " 87K", "  34 µs", "  67 µs", " 124 µs", " 256 µs", " 534 µs", " 891 µs", "    0", " 79%")],
        ["med", sampleRow("14:30:12", " 89K", "  33 µs", "  65 µs", " 121 µs", " 248 µs", " 512 µs", " 867 µs", "    0", " 79%")],
        ["med", sampleRow("14:30:13", " 88K", "  34 µs", "  66 µs", " 123 µs", " 252 µs", " 523 µs", " 878 µs", "    0", " 79%")],
        ["med", sampleRow("14:30:14", " 87K", "  34 µs", "  67 µs", " 124 µs", " 255 µs", " 531 µs", " 889 µs", "    0", " 79%")],
        ["med terminal-cursor", ""]
      ]
    },
    "zipf": {
      title: "valkey-lab --connections 16 --pipeline 32 --distribution zipf",
      lines: [
        ["", D("$") + " " + C("valkey-lab") + " --connections 16 --pipeline 32 --distribution zipf"],
        ["", ""],
        ["", B(C("valkey-lab, powered by cachecannon"))],
        ["", D("──────────────────")],
        ["", C("target") + "     127.0.0.1:6379 (Resp)"],
        ["", C("workload") + "   1M keys, 16B key, 64B value, 80:20 GET:SET, zipf"],
        ["", C("threads") + "    8"],
        ["", C("conns") + "      16, pipeline 32"],
        ["med", ""],
        ["med", "[precheck ok 8ms]"],
        ["med", "[warmup 10s]"],
        ["med", "[running 60s]"],
        ["med", ""],
        ["med", HDR],
        ["med", SEP],
        ["med", sampleRow("14:30:11", "487K", "  52 µs", "  98 µs", " 178 µs", " 356 µs", " 912 µs", "1.34 ms", "    0", " 92%")],
        ["med", sampleRow("14:30:12", "493K", "  51 µs", "  96 µs", " 174 µs", " 348 µs", " 878 µs", "1.28 ms", "    0", " 92%")],
        ["med", sampleRow("14:30:13", "485K", "  53 µs", "  99 µs", " 181 µs", " 362 µs", " 934 µs", "1.38 ms", "    0", " 91%")],
        ["med", sampleRow("14:30:14", "491K", "  51 µs", "  97 µs", " 176 µs", " 352 µs", " 898 µs", "1.31 ms", "    0", " 92%")],
        ["med terminal-cursor", ""]
      ]
    },
    "prefill": {
      title: "valkey-lab --prefill --ratio 100:0",
      lines: [
        ["", D("$") + " " + C("valkey-lab") + " --prefill --ratio 100:0"],
        ["", ""],
        ["", B(C("valkey-lab, powered by cachecannon"))],
        ["", D("──────────────────")],
        ["", C("target") + "     127.0.0.1:6379 (Resp)"],
        ["", C("workload") + "   1M keys, 16B key, 64B value, 100:0 GET:SET"],
        ["", C("threads") + "    8"],
        ["", C("conns") + "      1, pipeline 1"],
        ["med", ""],
        ["med", "[precheck ok 8ms]"],
        ["med", "[prefill 1M keys... done 4.2s]"],
        ["med", "[warmup 10s]"],
        ["med", "[running 60s]"],
        ["med", ""],
        ["med", HDR],
        ["med", SEP],
        ["med", sampleRow("14:30:11", " 87K", "  34 µs", "  67 µs", " 124 µs", " 256 µs", " 534 µs", " 891 µs", "    0", "100%")],
        ["med", sampleRow("14:30:12", " 89K", "  33 µs", "  65 µs", " 121 µs", " 248 µs", " 512 µs", " 867 µs", "    0", "100%")],
        ["med", sampleRow("14:30:13", " 88K", "  34 µs", "  66 µs", " 123 µs", " 252 µs", " 523 µs", " 878 µs", "    0", "100%")],
        ["med terminal-cursor", ""]
      ]
    },
    "ratelimit": {
      title: "valkey-lab --rate-limit 500000 --connections 16 --pipeline 32",
      lines: [
        ["", D("$") + " " + C("valkey-lab") + " --rate-limit 500000 --connections 16 --pipeline 32"],
        ["", ""],
        ["", B(C("valkey-lab, powered by cachecannon"))],
        ["", D("──────────────────")],
        ["", C("target") + "     127.0.0.1:6379 (Resp)"],
        ["", C("workload") + "   1M keys, 16B key, 64B value, 80:20 GET:SET"],
        ["", C("threads") + "    8"],
        ["", C("conns") + "      16, pipeline 32"],
        ["", C("ratelimit") + "  500K req/s"],
        ["med", ""],
        ["med", "[precheck ok 12ms]"],
        ["med", "[warmup 10s]"],
        ["med", "[running 60s]"],
        ["med", ""],
        ["med", HDR],
        ["med", SEP],
        ["med", sampleRow("14:30:11", "500K", "  41 µs", "  78 µs", " 145 µs", " 289 µs", " 612 µs", "1.02 ms", "    0", " 79%")],
        ["med", sampleRow("14:30:12", "500K", "  40 µs", "  76 µs", " 142 µs", " 284 µs", " 598 µs", " 978 µs", "    0", " 79%")],
        ["med", sampleRow("14:30:13", "500K", "  41 µs", "  77 µs", " 144 µs", " 287 µs", " 605 µs", "1.01 ms", "    0", " 79%")],
        ["med terminal-cursor", ""]
      ]
    },
    "saturate": {
      title: "valkey-lab saturate --slo-p999 1ms",
      lines: [
        ["", D("$") + " " + C("valkey-lab") + " saturate --slo-p999 1ms --connections 16 --pipeline 32"],
        ["", ""],
        ["", B(C("valkey-lab, powered by cachecannon"))],
        ["", D("──────────────────")],
        ["", C("target") + "     127.0.0.1:6379 (Resp)"],
        ["", C("conns") + "      16, pipeline 32"],
        ["slow", ""],
        ["slow", D("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━")],
        ["slow", G("STEP 1 — PASS")],
        ["slow", ""],
        ["slow", "SLO:    500K @ p999 ≤ 1ms"],
        ["slow", "Result: " + B("498K") + " @ p999=" + B("312 µs")],
        ["slow", D("Headroom: 69%")],
        ["slow", D("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━")],
        ["slow", ""],
        ["slow", D("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━")],
        ["slow", R("STEP 2 — FAIL — Throughput Limited")],
        ["slow", ""],
        ["slow", "SLO:    525K @ p999 ≤ 1ms"],
        ["slow", "Result: " + B("412K") + " @ p999=" + B("845 µs")],
        ["slow", Y("Throughput: 78% (need 90%)")],
        ["slow", D("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━")],
        ["slow", ""],
        ["slow", D("────────────────────────────────────────────────────────")],
        ["slow", G("MAX COMPLIANT THROUGHPUT: 498K req/s")]
      ]
    },
    "export": {
      title: "valkey-lab --parquet results.parquet",
      lines: [
        ["", D("$") + " " + C("valkey-lab") + " --parquet results.parquet --duration 5m --connections 16 --pipeline 32"],
        ["", ""],
        ["", B(C("valkey-lab, powered by cachecannon"))],
        ["", D("──────────────────")],
        ["", C("target") + "     127.0.0.1:6379 (Resp)"],
        ["", C("workload") + "   1M keys, 16B key, 64B value, 80:20 GET:SET"],
        ["", C("threads") + "    8"],
        ["", C("conns") + "      16, pipeline 32"],
        ["", C("output") + "     results.parquet"],
        ["med", ""],
        ["med", "[precheck ok 12ms]"],
        ["med", "[warmup 10s]"],
        ["med", "[running 5m]"],
        ["med", ""],
        ["med", HDR],
        ["med", SEP],
        ["med", sampleRow("14:30:11", "524K", "  48 µs", "  89 µs", " 156 µs", " 312 µs", " 891 µs", "1.24 ms", "    0", " 79%")],
        ["med", sampleRow("14:30:12", "531K", "  47 µs", "  87 µs", " 152 µs", " 298 µs", " 756 µs", "1.12 ms", "    0", " 79%")],
        ["med", D("  ...")],
        ["med", ""],
        ["med", G("[wrote 300 samples to results.parquet]")],
        ["med", ""],
        ["", D("$") + " " + C("valkey-lab") + " view results.parquet"],
        ["", D("  Serving on http://localhost:9090")]
      ]
    }
  };

  /* ---- Feature deep-dive scenarios ---- */
  var features = {
    "mixed": {
      title: "valkey-lab --connections 16 --pipeline 32 --ratio 80:20",
      lines: [
        ["", D("$") + " " + C("valkey-lab") + " --connections 16 --pipeline 32 --ratio 80:20"],
        ["", ""],
        ["", B(C("valkey-lab, powered by cachecannon"))],
        ["", D("──────────────────")],
        ["", C("target") + "     127.0.0.1:6379 (Resp)"],
        ["", C("workload") + "   1M keys, 16B key, 64B value, " + B("80:20 GET:SET")],
        ["", C("conns") + "      16, pipeline 32"],
        ["med", ""],
        ["med", "[precheck ok 12ms]"],
        ["med", "[running 60s]"],
        ["med", ""],
        ["med", HDR],
        ["med", SEP],
        ["med", sampleRow("14:30:11", "524K", "  48 µs", "  89 µs", " 156 µs", " 312 µs", " 891 µs", "1.24 ms", "    0", " 79%")],
        ["med", sampleRow("14:30:12", "531K", "  47 µs", "  87 µs", " 152 µs", " 298 µs", " 756 µs", "1.12 ms", "    0", " 79%")],
        ["med", sampleRow("14:30:13", "528K", "  47 µs", "  88 µs", " 154 µs", " 305 µs", " 812 µs", "1.18 ms", "    0", " 79%")],
        ["med terminal-cursor", ""]
      ]
    },
    "ratelimit": {
      title: "valkey-lab --rate-limit 500000 --connections 16 --pipeline 32",
      lines: [
        ["", D("$") + " " + C("valkey-lab") + " --rate-limit 500000 --connections 16 --pipeline 32"],
        ["", ""],
        ["", B(C("valkey-lab, powered by cachecannon"))],
        ["", D("──────────────────")],
        ["", C("target") + "     127.0.0.1:6379 (Resp)"],
        ["", C("workload") + "   1M keys, 16B key, 64B value, 80:20 GET:SET"],
        ["", C("conns") + "      16, pipeline 32"],
        ["", C("ratelimit") + "  " + B("500K req/s")],
        ["med", ""],
        ["med", "[precheck ok 12ms]"],
        ["med", "[running 60s]"],
        ["med", ""],
        ["med", HDR],
        ["med", SEP],
        ["med", sampleRow("14:30:11", "500K", "  41 µs", "  78 µs", " 145 µs", " 289 µs", " 612 µs", "1.02 ms", "    0", " 79%")],
        ["med", sampleRow("14:30:12", "500K", "  40 µs", "  76 µs", " 142 µs", " 284 µs", " 598 µs", " 978 µs", "    0", " 79%")],
        ["med", sampleRow("14:30:13", "500K", "  41 µs", "  77 µs", " 144 µs", " 287 µs", " 605 µs", "1.01 ms", "    0", " 79%")],
        ["med terminal-cursor", ""]
      ]
    },
    "prefill": {
      title: "valkey-lab --prefill --ratio 100:0",
      lines: [
        ["", D("$") + " " + C("valkey-lab") + " --prefill --ratio 100:0"],
        ["", ""],
        ["", B(C("valkey-lab, powered by cachecannon"))],
        ["", D("──────────────────")],
        ["", C("target") + "     127.0.0.1:6379 (Resp)"],
        ["", C("workload") + "   1M keys, 16B key, 64B value, 100:0 GET:SET"],
        ["", C("conns") + "      1, pipeline 1"],
        ["med", ""],
        ["med", "[precheck ok 8ms]"],
        ["med", B("[prefill 1M keys... done 4.2s]")],
        ["med", "[warmup 10s]"],
        ["med", "[running 60s]"],
        ["med", ""],
        ["med", HDR],
        ["med", SEP],
        ["med", sampleRow("14:30:11", " 87K", "  34 µs", "  67 µs", " 124 µs", " 256 µs", " 534 µs", " 891 µs", "    0", B("100%"))],
        ["med", sampleRow("14:30:12", " 89K", "  33 µs", "  65 µs", " 121 µs", " 248 µs", " 512 µs", " 867 µs", "    0", B("100%"))],
        ["med", sampleRow("14:30:13", " 88K", "  34 µs", "  66 µs", " 123 µs", " 252 µs", " 523 µs", " 878 µs", "    0", B("100%"))],
        ["med terminal-cursor", ""]
      ]
    },
    "taillatency": {
      title: "valkey-lab --connections 16 --pipeline 32 --duration 60s",
      lines: [
        ["", D("$") + " " + C("valkey-lab") + " --connections 16 --pipeline 32 --duration 60s"],
        ["", ""],
        ["", B(C("valkey-lab, powered by cachecannon"))],
        ["", D("──────────────────")],
        ["", C("target") + "     127.0.0.1:6379 (Resp)"],
        ["", C("conns") + "      16, pipeline 32"],
        ["med", ""],
        ["med", "[running 60s]"],
        ["med", ""],
        ["med", HDR],
        ["med", SEP],
        ["med", sampleRow("14:30:11", "528K", "  47 µs", "  88 µs", " 154 µs", " 305 µs", " 812 µs", "1.18 ms", "    0", " 79%")],
        ["med", sampleRow("14:30:12", "530K", "  47 µs", "  87 µs", " 153 µs", " 301 µs", " 778 µs", "1.15 ms", "    0", " 79%")],
        ["med", sampleRow("14:30:13", "524K", "  48 µs", "  89 µs", " 156 µs", " 312 µs", " 891 µs", "1.24 ms", "    0", " 79%")],
        ["med", D("14:30:14") + "  " + B("519K") + "  " + D("  49 µs") + "  " + D("  91 µs") + "  " + D(" 168 µs") + "  " + R(" 2.4 ms") + "  " + R(" 8.9 ms") + "  " + R("12.3 ms") + "  " + D("    0") + "  " + D(" 79%")],
        ["med", sampleRow("14:30:15", "531K", "  47 µs", "  87 µs", " 152 µs", " 298 µs", " 756 µs", "1.12 ms", "    0", " 79%")],
        ["med", D("                                                " ) + R("^ spike")],
        ["med terminal-cursor", ""]
      ]
    },
    "zipf": {
      title: "valkey-lab --connections 16 --pipeline 32 --distribution zipf",
      lines: [
        ["", D("$") + " " + C("valkey-lab") + " --connections 16 --pipeline 32 --distribution zipf"],
        ["", ""],
        ["", B(C("valkey-lab, powered by cachecannon"))],
        ["", D("──────────────────")],
        ["", C("target") + "     127.0.0.1:6379 (Resp)"],
        ["", C("workload") + "   1M keys, 16B key, 64B value, 80:20 GET:SET, " + B("zipf")],
        ["", C("conns") + "      16, pipeline 32"],
        ["med", ""],
        ["med", "[precheck ok 12ms]"],
        ["med", "[running 60s]"],
        ["med", ""],
        ["med", HDR],
        ["med", SEP],
        ["med", sampleRow("14:30:11", "487K", "  52 µs", "  98 µs", " 178 µs", " 356 µs", " 912 µs", "1.34 ms", "    0", " 92%")],
        ["med", sampleRow("14:30:12", "493K", "  51 µs", "  96 µs", " 174 µs", " 348 µs", " 878 µs", "1.28 ms", "    0", " 92%")],
        ["med", sampleRow("14:30:13", "485K", "  53 µs", "  99 µs", " 181 µs", " 362 µs", " 934 µs", "1.38 ms", "    0", " 91%")],
        ["med terminal-cursor", ""]
      ]
    },
    "parquet": {
      title: "valkey-lab --parquet results.parquet",
      lines: [
        ["", D("$") + " " + C("valkey-lab") + " --parquet results.parquet --duration 5m --connections 16 --pipeline 32"],
        ["", ""],
        ["", B(C("valkey-lab, powered by cachecannon"))],
        ["", D("──────────────────")],
        ["", C("target") + "     127.0.0.1:6379 (Resp)"],
        ["", C("conns") + "      16, pipeline 32"],
        ["", C("output") + "     " + B("results.parquet")],
        ["med", ""],
        ["med", "[running 5m]"],
        ["med", ""],
        ["med", HDR],
        ["med", SEP],
        ["med", sampleRow("14:30:11", "524K", "  48 µs", "  89 µs", " 156 µs", " 312 µs", " 891 µs", "1.24 ms", "    0", " 79%")],
        ["med", sampleRow("14:30:12", "531K", "  47 µs", "  87 µs", " 152 µs", " 298 µs", " 756 µs", "1.12 ms", "    0", " 79%")],
        ["med", D("  ...")],
        ["med", ""],
        ["med", G("[wrote 300 samples to results.parquet]")],
        ["med", ""],
        ["", D("$") + " " + C("valkey-lab") + " view results.parquet"],
        ["", D("  Serving on http://localhost:9090")]
      ]
    }
  };

  /* ---- Copy to clipboard ---- */
  var copyBtn = document.getElementById("copy-btn");
  var installCmd = document.getElementById("install-cmd");
  if (copyBtn && installCmd) {
    copyBtn.addEventListener("click", function () {
      navigator.clipboard.writeText(installCmd.value || installCmd.textContent).then(function () {
        copyBtn.classList.add("copied");
        copyBtn.innerHTML = '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"/></svg>';
        setTimeout(function () {
          copyBtn.classList.remove("copied");
          copyBtn.innerHTML = '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 01-2-2V4a2 2 0 012-2h9a2 2 0 012 2v1"/></svg>';
        }, 2000);
      });
    });
  }

  /* ---- Nav scroll ---- */
  var nav = document.querySelector(".nav");
  if (nav) {
    window.addEventListener("scroll", function () {
      nav.classList.toggle("scrolled", window.scrollY > 10);
    });
  }

  /* ---- Theme toggle ---- */
  var toggle = document.getElementById("theme-toggle");
  var root = document.documentElement;
  function setTheme(t) {
    root.setAttribute("data-theme", t);
    localStorage.setItem("theme", t);
    if (toggle) toggle.textContent = t === "dark" ? "\u263E" : "\u2600";
  }
  setTheme(localStorage.getItem("theme") || "dark");
  if (toggle) {
    toggle.addEventListener("click", function () {
      setTheme(root.getAttribute("data-theme") === "dark" ? "light" : "dark");
    });
  }

  /* ---- Scroll fade-up ---- */
  var fadeObs = new IntersectionObserver(function (entries) {
    entries.forEach(function (e) {
      if (e.isIntersecting) { e.target.classList.add("visible"); fadeObs.unobserve(e.target); }
    });
  }, { threshold: 0.08 });
  document.querySelectorAll(".fade-up").forEach(function (el) { fadeObs.observe(el); });

  /* ---- Smooth anchor scroll ---- */
  document.querySelectorAll('a[href^="#"]').forEach(function (a) {
    a.addEventListener("click", function (e) {
      var t = document.querySelector(a.getAttribute("href"));
      if (t) { e.preventDefault(); t.scrollIntoView({ behavior: "smooth" }); }
    });
  });

  /* ---- Terminal typewriter animation ---- */
  function animateTerminal(el) {
    var body = el.querySelector(".terminal-body");
    if (!body || body.dataset.done) return;
    body.dataset.done = "1";
    var lines = body.querySelectorAll(".line");
    lines.forEach(function (l) { l.style.opacity = "0"; });
    var base = 40;
    lines.forEach(function (line, i) {
      var d = line.classList.contains("slow") ? base + i * 160
            : line.classList.contains("med")  ? base + i * 100
            : base + i * 55;
      setTimeout(function () { line.style.opacity = "1"; line.classList.add("visible"); }, d);
    });
  }

  var termObs = new IntersectionObserver(function (entries) {
    entries.forEach(function (e) {
      if (e.isIntersecting) { animateTerminal(e.target); termObs.unobserve(e.target); }
    });
  }, { threshold: 0.15 });
  document.querySelectorAll(".terminal[data-animate]").forEach(function (t) { termObs.observe(t); });

  /* ---- Shared render function ---- */
  function renderTerminal(data, key, bodyEl, titleEl) {
    var s = data[key];
    if (!s || !bodyEl) return;
    if (titleEl) titleEl.textContent = s.title;
    var html = "";
    for (var i = 0; i < s.lines.length; i++) {
      html += L(s.lines[i][0], s.lines[i][1]);
    }
    bodyEl.innerHTML = html;
    var lines = bodyEl.querySelectorAll(".line");
    var base = 40;
    lines.forEach(function (line, i) {
      line.style.opacity = "0";
      var d = line.classList.contains("slow") ? base + i * 160
            : line.classList.contains("med")  ? base + i * 100
            : base + i * 55;
      setTimeout(function () { line.style.opacity = "1"; line.classList.add("visible"); }, d);
    });
  }

  function initPicker(cards, data, attrName, bodyEl, titleEl, defaultKey) {
    renderTerminal(data, defaultKey, bodyEl, titleEl);
    cards.forEach(function (card) {
      card.addEventListener("click", function () {
        cards.forEach(function (c) { c.classList.remove("active"); });
        card.classList.add("active");
        renderTerminal(data, card.dataset[attrName], bodyEl, titleEl);
      });
    });
  }

  /* ---- Scenario picker (Get Started) ---- */
  initPicker(
    document.querySelectorAll("[data-scenario]"),
    scenarios, "scenario",
    document.getElementById("scenario-body"),
    document.getElementById("scenario-title"),
    "default"
  );

  /* ---- Feature picker (Deep Dive) ---- */
  initPicker(
    document.querySelectorAll("[data-feature]"),
    features, "feature",
    document.getElementById("feature-body"),
    document.getElementById("feature-title"),
    "mixed"
  );
})();
