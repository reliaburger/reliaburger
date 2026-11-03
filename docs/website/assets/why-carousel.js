// One of the site's three scripts: it turns the "Built differently" list into
// a slideshow that shows one difference at a time. Without JavaScript the list
// stays as it is, all five stacked. It moves on every nine seconds. Pause and
// Play always do what they say. Rotation also stops when keyboard focus comes
// in from outside (it stays stopped until Play), while a mouse rests on it,
// while the tab is hidden, and for anyone who prefers reduced motion. Touch
// doesn't count as hovering: a phone fires a mouse-enter on every tap and no
// mouse-leave, which used to freeze the slideshow behind a Play that did
// nothing. It follows the WAI-ARIA carousel pattern: the slide text is a polite
// live region only while it isn't rotating by itself, so a screen reader isn't
// interrupted every nine seconds.
(function () {
  "use strict";

  var INTERVAL = 9000;
  var root = document.querySelector("[data-carousel]");
  if (!root) return;
  var list = root.querySelector(".why-slides");
  var slides = Array.prototype.slice.call(list.children);
  if (slides.length < 2) return;

  var motion = window.matchMedia ? window.matchMedia("(prefers-reduced-motion: reduce)") : null;
  var current = 0;
  var timer = null;
  var stopped = false;
  var hovered = false;

  function title(slide) {
    return slide.querySelector(".why-punchline").textContent.trim();
  }

  function button(className, text, label) {
    var element = document.createElement("button");
    element.type = "button";
    element.className = className;
    element.textContent = text;
    if (label) element.setAttribute("aria-label", label);
    return element;
  }

  root.classList.add("is-enhanced");
  root.setAttribute("role", "region");
  root.setAttribute("aria-roledescription", "carousel");
  root.setAttribute("aria-label", "How Reliaburger is built differently");
  // The slides become groups, so the list stops being a list, and the live
  // region goes on a wrapper: an ARIA attribute on the list itself would
  // bring its list role back.
  var viewport = document.createElement("div");
  viewport.className = "why-viewport";
  list.parentNode.insertBefore(viewport, list);
  viewport.appendChild(list);
  list.setAttribute("role", "none");
  slides.forEach(function (slide, index) {
    slide.setAttribute("role", "group");
    slide.setAttribute("aria-roledescription", "slide");
    slide.setAttribute("aria-label", index + 1 + " of " + slides.length);
  });

  var controls = document.createElement("div");
  controls.className = "why-controls";
  var previous = button("why-step", "‹", "Previous difference");
  var next = button("why-step", "›", "Next difference");
  var toggle = button("why-toggle", "Pause");
  var dots = document.createElement("div");
  dots.className = "why-dots";
  dots.setAttribute("role", "group");
  dots.setAttribute("aria-label", "Choose a difference");
  var dotButtons = slides.map(function (slide, index) {
    var dot = button("why-dot", "", index + 1 + ": " + title(slide));
    dot.addEventListener("click", function () {
      show(index);
    });
    dots.appendChild(dot);
    return dot;
  });
  controls.appendChild(previous);
  controls.appendChild(dots);
  controls.appendChild(next);
  controls.appendChild(toggle);
  root.appendChild(controls);

  function show(index) {
    current = (index + slides.length) % slides.length;
    slides.forEach(function (slide, i) {
      slide.classList.toggle("is-active", i === current);
    });
    dotButtons.forEach(function (dot, i) {
      if (i === current) dot.setAttribute("aria-current", "true");
      else dot.removeAttribute("aria-current");
    });
    schedule();
  }

  function reduced() {
    return Boolean(motion && motion.matches);
  }

  function rotating() {
    return !stopped && !hovered && !reduced() && !document.hidden;
  }

  // Every change of state comes through here, so the timer restarts from a
  // full interval after any pause or manual step.
  function schedule() {
    clearTimeout(timer);
    timer = null;
    var on = rotating();
    if (on) {
      timer = setTimeout(function () {
        show(current + 1);
      }, INTERVAL);
    }
    viewport.setAttribute("aria-live", on ? "off" : "polite");
    toggle.hidden = reduced();
    toggle.textContent = stopped ? "Play" : "Pause";
    toggle.setAttribute("aria-pressed", stopped ? "true" : "false");
  }

  previous.addEventListener("click", function () {
    show(current - 1);
  });
  next.addEventListener("click", function () {
    show(current + 1);
  });
  toggle.addEventListener("click", function () {
    stopped = !stopped;
    // Play means play, even with the mouse still over the button.
    if (!stopped) hovered = false;
    schedule();
  });
  root.addEventListener("keydown", function (event) {
    if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
    event.preventDefault();
    show(current + (event.key === "ArrowLeft" ? -1 : 1));
    if (dots.contains(document.activeElement)) dotButtons[current].focus();
  });
  root.addEventListener("pointerenter", function (event) {
    if (event.pointerType !== "mouse") return;
    hovered = true;
    schedule();
  });
  root.addEventListener("pointerleave", function (event) {
    if (event.pointerType !== "mouse") return;
    hovered = false;
    schedule();
  });
  // Keyboard focus arriving from outside stops the slideshow until the visitor
  // presses Play. A click focuses too, so only keyboard focus (:focus-visible)
  // counts, or pressing Play with the mouse would stop it again.
  root.addEventListener("focusin", function (event) {
    if (root.contains(event.relatedTarget)) return;
    if (!event.target.matches(":focus-visible")) return;
    stopped = true;
    schedule();
  });
  document.addEventListener("visibilitychange", schedule);
  if (motion && motion.addEventListener) motion.addEventListener("change", schedule);

  show(0);
})();
