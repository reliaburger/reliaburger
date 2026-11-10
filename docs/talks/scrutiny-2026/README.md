# Scrutiny, not popularity

A short deck (eight slides) on the design goal in the whitepaper's
[§2](../../whitepaper.md#trust-from-scrutiny-not-popularity): trust an
orchestrator for what anyone can check, not for how many people run it.

`deck.html` is the whole deck: one file, no build step, nothing loaded from
anywhere else. Open it in a browser. The arrow keys, Page Up and Page Down
move between slides, Home and End jump to either end, and N shows the speaker
notes. The address bar keeps the slide number (`deck.html#3`), and printing
gives one slide a page with its notes under it.

Every number on a slide has its source on the last slide, and the notes give
each one's method and caveats. The Reliaburger numbers were measured on
10 November 2026 on the 0.2.8 train; re-measure them before you give the talk
(`make loc`, `cargo tree -e normal --target x86_64-unknown-linux-gnu`).
