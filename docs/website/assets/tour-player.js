// The only script on this site: it plays the tour's recording with the
// vendored asciinema player (assets/asciinema/). The player and the recording
// load from this site, and only when someone opens the tour. Without
// JavaScript the <noscript> link and the written tour still work.
(function () {
  "use strict";

  var figure = document.getElementById("tour-recording");
  var tour = figure && figure.closest("details");
  if (!figure || !tour) return;

  var screen = figure.querySelector(".recording-screen");
  var speeds = figure.querySelector(".recording-speed");
  var cast = figure.getAttribute("data-cast");
  var poster = figure.getAttribute("data-poster");
  var player = null;
  var speed = 1;
  var started = false;

  function load(tag, attributes) {
    return new Promise(function (resolve, reject) {
      var element = document.createElement(tag);
      Object.keys(attributes).forEach(function (name) {
        element.setAttribute(name, attributes[name]);
      });
      element.onload = resolve;
      element.onerror = reject;
      document.head.appendChild(element);
    });
  }

  // The player takes its speed when it's created, so a new speed means a new
  // player that picks up where the old one was.
  function create(startAt, play) {
    if (player) player.dispose();
    player = window.AsciinemaPlayer.create(cast, screen, {
      poster: startAt ? undefined : poster,
      startAt: startAt || undefined,
      autoPlay: play,
      preload: true,
      speed: speed,
      fit: "width",
      terminalFontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
    });
    var playing = play;
    player.addEventListener("play", function () { playing = true; });
    player.addEventListener("pause", function () { playing = false; });
    player.addEventListener("ended", function () { playing = false; });
    player.isPlaying = function () { return playing; };
  }

  function setSpeed(button) {
    speed = Number(button.getAttribute("data-speed"));
    speeds.querySelectorAll("button").forEach(function (other) {
      other.setAttribute("aria-pressed", String(other === button));
    });
    if (!player) return;
    var at = player.getCurrentTime();
    create(at > 0 ? at : 0, player.isPlaying());
  }

  function start() {
    if (started || !tour.open) return;
    started = true;
    Promise.all([
      load("link", { rel: "stylesheet", href: "./assets/asciinema/asciinema-player.css" }),
      load("script", { src: "./assets/asciinema/asciinema-player.min.js" }),
    ]).then(function () {
      figure.classList.add("has-player");
      speeds.hidden = false;
      create(0, false);
    }).catch(function () {
      started = false;
    });
  }

  speeds.addEventListener("click", function (event) {
    var button = event.target.closest("button[data-speed]");
    if (button) setSpeed(button);
  });
  tour.addEventListener("toggle", start);
  start();
})();
