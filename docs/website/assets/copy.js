// Copy buttons for every command snippet, copy-on-click for the heading
// permalinks, and "Copy as Markdown" and "Open in Claude" for the page's
// Markdown copy. The buttons come from this script, so without JavaScript
// there's no dead button: the snippets stay plain, selectable text, the `#`
// links still navigate and the alternate link still names the copy.
(function () {
  "use strict";

  // One polite live region announces every copy to screen readers.
  var status = document.createElement("span");
  status.className = "visually-hidden";
  status.setAttribute("aria-live", "polite");
  document.body.appendChild(status);

  function announce(message) {
    status.textContent = "";
    // A fresh text node after a tick, so the same message is read again.
    setTimeout(function () { status.textContent = message; }, 50);
  }

  // The Clipboard API needs a secure context (https or localhost). A page
  // opened from disk falls back to selecting the text and the old command.
  function copyText(text, fallbackNode) {
    if (navigator.clipboard && window.isSecureContext) {
      return navigator.clipboard.writeText(text);
    }
    return new Promise(function (resolve, reject) {
      var range = document.createRange();
      range.selectNodeContents(fallbackNode);
      var selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      var copied = false;
      try { copied = document.execCommand("copy"); } catch (e) { copied = false; }
      selection.removeAllRanges();
      if (copied) resolve(); else reject(new Error("copy refused"));
    });
  }

  function addCopyButton(pre) {
    var wrapper = document.createElement("div");
    wrapper.className = "copyable";
    pre.parentNode.insertBefore(wrapper, pre);
    wrapper.appendChild(pre);

    var button = document.createElement("button");
    button.type = "button";
    button.className = "copy";
    button.textContent = "Copy";
    button.setAttribute("aria-label", "Copy command");
    wrapper.appendChild(button);

    var timer = null;
    button.addEventListener("click", function () {
      var text = pre.textContent.trim();
      copyText(text, pre).then(function () {
        button.textContent = "Copied";
        announce("Copied to the clipboard");
      }, function () {
        // Leave the text selected so Ctrl-C or Cmd-C finishes the job.
        var range = document.createRange();
        range.selectNodeContents(pre);
        window.getSelection().removeAllRanges();
        window.getSelection().addRange(range);
        button.textContent = "Press Ctrl-C";
        announce("Couldn't copy; the command is selected");
      });
      clearTimeout(timer);
      timer = setTimeout(function () { button.textContent = "Copy"; }, 2000);
    });
  }

  var snippets = document.querySelectorAll("pre");
  for (var i = 0; i < snippets.length; i++) {
    addCopyButton(snippets[i]);
  }

  // A permalink still navigates, which updates the address bar; it also
  // copies the full address, ready to paste into a chat.
  document.addEventListener("click", function (event) {
    var permalink = event.target.closest("a.permalink");
    if (!permalink) return;
    copyText(permalink.href, permalink).then(function () {
      permalink.setAttribute("data-copied", "");
      announce("Link copied");
      setTimeout(function () { permalink.removeAttribute("data-copied"); }, 2000);
    }, function () {});
  });

  // "Copy as Markdown" and "Open in Claude" (#1695), for the Markdown copy
  // the page names in its alternate link. Without this script the page has
  // neither button, and the alternate link and llms.txt still lead to it.
  function addPageActions() {
    var alternate = document.querySelector('link[rel="alternate"][type="text/markdown"]');
    var main = document.querySelector("main");
    if (!alternate || !main) return;
    var markdown = new URL(alternate.getAttribute("href"), document.baseURI).href;

    var actions = document.createElement("div");
    actions.className = "page-actions";

    var copy = document.createElement("button");
    copy.type = "button";
    copy.textContent = "Copy as Markdown";
    var timer = null;
    copy.addEventListener("click", function () {
      var fetched = fetch(markdown).then(function (response) {
        if (!response.ok) throw new Error("status " + response.status);
        return response.text();
      });
      var copied;
      // Safari lets a click write to the clipboard only straight away, so
      // it gets the download as a promise rather than after it arrives.
      if (navigator.clipboard && navigator.clipboard.write && window.ClipboardItem && window.isSecureContext) {
        copied = navigator.clipboard.write([new ClipboardItem({
          "text/plain": fetched.then(function (text) { return new Blob([text], { type: "text/plain" }); })
        })]);
      } else {
        copied = fetched.then(function (text) {
          var holder = document.createElement("pre");
          holder.className = "visually-hidden";
          holder.textContent = text;
          document.body.appendChild(holder);
          return copyText(text, holder).then(function () { holder.remove(); },
            function (error) { holder.remove(); throw error; });
        });
      }
      copied.then(function () {
        copy.textContent = "Copied";
        announce("The page's Markdown is on the clipboard");
      }, function () {
        copy.textContent = "Couldn't copy";
        announce("Couldn't copy the Markdown; it's at " + markdown);
      });
      clearTimeout(timer);
      timer = setTimeout(function () { copy.textContent = "Copy as Markdown"; }, 2000);
    });

    var open = document.createElement("a");
    open.textContent = "Open in Claude";
    open.target = "_blank";
    open.rel = "noopener";
    open.href = "https://claude.ai/new?q=" + encodeURIComponent(
      "Read " + markdown + " so we can talk about it.");

    actions.appendChild(copy);
    actions.appendChild(open);
    main.insertBefore(actions, main.firstChild);
  }
  addPageActions();

  // The tour's commands sit in a <details>. A link to them (#tour-commands)
  // should open it, not land on a closed box; #tour itself is always open.
  function openLinkedCommands() {
    var commands = document.getElementById("tour-commands");
    if (commands && location.hash === "#tour-commands") commands.open = true;
  }
  window.addEventListener("hashchange", openLinkedCommands);
  openLinkedCommands();
})();
