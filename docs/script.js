/* valkey-lab — page interactions + terminal animation */
(function () {
  "use strict";

  /* Nav scroll */
  var nav = document.querySelector(".nav");
  if (nav) {
    window.addEventListener("scroll", function () {
      nav.classList.toggle("scrolled", window.scrollY > 10);
    });
  }

  /* Theme toggle */
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

  /* Scroll fade-up */
  var fadeObs = new IntersectionObserver(function (entries) {
    entries.forEach(function (e) {
      if (e.isIntersecting) { e.target.classList.add("visible"); fadeObs.unobserve(e.target); }
    });
  }, { threshold: 0.08 });
  document.querySelectorAll(".fade-up").forEach(function (el) { fadeObs.observe(el); });

  /* Smooth anchor scroll */
  document.querySelectorAll('a[href^="#"]').forEach(function (a) {
    a.addEventListener("click", function (e) {
      var t = document.querySelector(a.getAttribute("href"));
      if (t) { e.preventDefault(); t.scrollIntoView({ behavior: "smooth" }); }
    });
  });

  /* Terminal typewriter animation */
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
})();
