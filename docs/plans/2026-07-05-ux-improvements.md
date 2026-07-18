# User Experience improvements

The goal of this task it to improve the learning curve, as well as demonstrability of the platform.

We want to implement new features:
1) relish setup - checks to see if there is a bun installed, and guides the user through the options to install it
2) relish manual - starts from a freshly installed cluster, and teaches the user how to use all the functionality shipped with it through a series of in-depth examples
3) relish source - embeds the source code that it was compiled from, and allows for fuzzy search and browsing in the terminal
4) revamt the README.md file (top level) to sell the project better


## relish setup

This is to guide the user on how to install bun
1) check for the latest version on github,
2) downloading the binary from github release,
3) exec that version if newer
4) generate the configuration based on the user answers

## relish manual

This feature needs to be a self-contained, up-to-date reference of every feature that Reliaburger ships with.

* it needs to cover working examples that someone with a fresh Reliaburger install can use to learn all the features of the platform
* it needs to have a search feature
* it needs to be able to write examples to current working directory
* the documentation shoudl be written to docs/manual/01_chapter.md
* it should be in Markdown format, but rendered in the terminal
* it should be brief (no unnecessary prose), but cover every feature 
- relish manual --web - should start a web server with the docs compiled into a single page html for the user to peruse in the browser

## relish source

Similar to relish manual, I'd like the binary to also ship all the app source code that it was compiled with, and be able to serach it

- relish source ebpf - should open the search view with ebpf prefilled


## README improvements

Currently README doesn't sell the project very well. We should make some tweaks:

* Current status should go away - it doesn't belong on the top level
* The repo layout should move to the manual
* the first section should sell all the features, and what the project solves
* the book should be featured more prominently
* waht's inside belongs to the manual as well
* we should feature the new relish manual - maybe I'll add some ascii cinema as well
