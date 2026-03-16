/* ============================================================
   valkey-lab — Terminal animations + page interactions
   ============================================================ */

(function () {
  "use strict";

  // ---- Nav scroll ----
  var nav = document.querySelector("nav");
  if (nav) {
    window.addEventListener("scroll", function () {
      nav.classList.toggle("scrolled", window.scrollY > 16);
    });
  }

  // ---- Theme toggle ----
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

  // ---- Scroll fade-up ----
  var fadeObserver = new IntersectionObserver(
    function (entries) {
      entries.forEach(function (e) {
        if (e.isIntersecting) {
          e.target.classList.add("visible");
          fadeObserver.unobserve(e.target);
        }
      });
    },
    { threshold: 0.08 }
  );
  document.querySelectorAll(".fade-up").forEach(function (el) {
    fadeObserver.observe(el);
  });

  // ---- Smooth scroll anchors ----
  document.querySelectorAll('a[href^="#"]').forEach(function (link) {
    link.addEventListener("click", function (e) {
      var target = document.querySelector(link.getAttribute("href"));
      if (target) {
        e.preventDefault();
        target.scrollIntoView({ behavior: "smooth" });
      }
    });
  });

  // ================================================================
  // Terminal typewriter animation
  // ================================================================
  // Each terminal with [data-animate] will type out its lines
  // sequentially when it scrolls into view.

  function animateTerminal(terminalEl) {
    var body = terminalEl.querySelector(".terminal-body");
    if (!body || body.dataset.animated) return;
    body.dataset.animated = "1";

    var lines = body.querySelectorAll(".line");
    if (!lines.length) return;

    // Hide all lines first
    lines.forEach(function (l) { l.style.opacity = "0"; });

    var baseDelay = 60; // ms between lines

    lines.forEach(function (line, i) {
      // Stagger: config block fast, data rows moderate, saturation slower
      var delay;
      if (line.classList.contains("slow")) {
        delay = baseDelay + i * 180;
      } else if (line.classList.contains("med")) {
        delay = baseDelay + i * 120;
      } else {
        delay = baseDelay + i * 70;
      }

      setTimeout(function () {
        line.style.opacity = "1";
        line.classList.add("visible");
      }, delay);
    });
  }

  var terminalObserver = new IntersectionObserver(
    function (entries) {
      entries.forEach(function (e) {
        if (e.isIntersecting) {
          animateTerminal(e.target);
          terminalObserver.unobserve(e.target);
        }
      });
    },
    { threshold: 0.2 }
  );

  document.querySelectorAll(".terminal[data-animate]").forEach(function (t) {
    terminalObserver.observe(t);
  });
})();
