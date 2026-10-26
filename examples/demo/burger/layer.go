package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"strings"
)

// layerOrder is the burger from the top down. Each name is also the layer's
// app, and so its service name on the cluster.
var layerOrder = []string{"top-bun", "lettuce", "tomato", "cheese", "patty", "bottom-bun"}

// look is how one version of a layer is drawn.
type look struct {
	Style string // what the version is, e.g. "cheddar"
	// The terminal line: 256-colour codes, a body pattern 24 characters
	// wide (spaces show the background), and whether the body is filled.
	Background, Foreground int
	Pattern                string
	// The flat SVG artwork's colours.
	Fill, Accent string
	// Broken versions fail their health check, so a rollout of one fails.
	Broken bool
}

// shape is what every version of a layer has in common.
type shape struct {
	Left, Right string // the ends of the terminal line
	Filled      bool   // body drawn on its background colour, or as coloured text
	Height      int    // SVG height
	draw        func(look) string
}

var shapes = map[string]shape{
	"top-bun":    {Left: "▟", Right: "▙", Filled: true, Height: 56, draw: drawTopBun},
	"lettuce":    {Left: "~", Right: "~", Height: 16, draw: drawLettuce},
	"tomato":     {Left: "▐", Right: "▌", Filled: true, Height: 20, draw: drawTomato},
	"cheese":     {Left: "◢", Right: "◣", Filled: true, Height: 22, draw: drawCheese},
	"patty":      {Left: "▐", Right: "▌", Filled: true, Height: 30, draw: drawPatty},
	"bottom-bun": {Left: "▜", Right: "▛", Filled: true, Height: 28, draw: drawBottomBun},
}

// recipes holds every layer's versions. v2 is a different take on the
// layer; v3 of the patty is the deliberately broken one.
var recipes = map[string]map[string]look{
	"top-bun": {
		"v1": {Style: "sesame", Background: 172, Foreground: 230, Pattern: "  '  .  '  .  '  .  '  .", Fill: "#d9893b", Accent: "#fbefd3"},
		"v2": {Style: "brioche", Background: 136, Foreground: 179, Pattern: "                        ", Fill: "#b8642a", Accent: "#e2a457"},
		"v3": {Style: "charcoal", Background: 237, Foreground: 250, Pattern: "  .  '  .  '  .  '  .  '", Fill: "#3a3a3a", Accent: "#d0d0d0"},
	},
	"lettuce": {
		"v1": {Style: "iceberg", Background: 76, Pattern: "^~^~~^~^~~^~^~~^~^~~^~^~", Fill: "#6cc04a", Accent: "#4c9a2e"},
		"v2": {Style: "rocket", Background: 28, Pattern: "~v~~v~~v~~v~~v~~v~~v~~v~", Fill: "#2f7d32", Accent: "#1f5a22"},
		"v3": {Style: "radicchio", Background: 125, Pattern: "^^~~^^~~^^~~^^~~^^~~^^~~", Fill: "#a1305f", Accent: "#f0d6e0"},
	},
	"tomato": {
		"v1": {Style: "beef tomato", Background: 160, Foreground: 217, Pattern: "  o    o    o    o    o ", Fill: "#d9372b", Accent: "#f6a59b"},
		"v2": {Style: "yellow", Background: 178, Foreground: 230, Pattern: "  o    o    o    o    o ", Fill: "#e8b923", Accent: "#fff2b8"},
		"v3": {Style: "green", Background: 64, Foreground: 150, Pattern: " o  o  o  o  o  o  o  o ", Fill: "#5f8f2a", Accent: "#c4e39a"},
	},
	"cheese": {
		"v1": {Style: "cheddar", Background: 220, Foreground: 214, Pattern: "   ▾       ▾       ▾    ", Fill: "#f7c331", Accent: "#e59a12"},
		"v2": {Style: "blue cheese", Background: 152, Foreground: 67, Pattern: "  ∙ ▾  ∙    ▾ ∙   ∙  ▾  ", Fill: "#a9d3e0", Accent: "#4f7f9c"},
		"v3": {Style: "red leicester", Background: 208, Foreground: 166, Pattern: "   ▾       ▾       ▾    ", Fill: "#ee7d22", Accent: "#c45a10"},
	},
	"patty": {
		"v1": {Style: "beef", Background: 94, Foreground: 52, Pattern: "░▒░▒░▒░▒░▒░▒░▒░▒░▒░▒░▒░▒", Fill: "#7a4424", Accent: "#4a2614"},
		"v2": {Style: "veggie", Background: 64, Foreground: 22, Pattern: "░ ░ ░ ░ ░ ░ ░ ░ ░ ░ ░ ░ ", Fill: "#6b8b35", Accent: "#3f5a1c"},
		"v3": {Style: "raw", Background: 211, Foreground: 168, Pattern: "▒░▒░▒░▒░▒░▒░▒░▒░▒░▒░▒░▒░", Fill: "#e98ba0", Accent: "#c45570", Broken: true},
	},
	"bottom-bun": {
		"v1": {Style: "sesame", Background: 172, Foreground: 172, Pattern: "                        ", Fill: "#d9893b", Accent: "#b9702c"},
		"v2": {Style: "brioche", Background: 136, Foreground: 179, Pattern: "  ‾‾‾‾‾‾‾‾‾‾‾‾‾‾‾‾‾‾‾‾  ", Fill: "#b8642a", Accent: "#e2a457"},
		"v3": {Style: "charcoal", Background: 237, Foreground: 237, Pattern: "                        ", Fill: "#3a3a3a", Accent: "#555555"},
	},
}

// Slice is what a layer serves at GET /layer: its version, the instance
// that served it, and its own drawing, so a new version changes the burger
// without the counter knowing anything about it.
type Slice struct {
	Layer    string `json:"layer"`
	Version  string `json:"version"`
	Style    string `json:"style"`
	Node     string `json:"node"`
	Instance string `json:"instance"`
	Text     string `json:"text"`   // one terminal line, ANSI-coloured
	SVG      string `json:"svg"`    // an SVG <g>, 260 wide, drawn from 0,0
	Height   int    `json:"height"` // the SVG's height
}

type layer struct {
	name, version, node, instance string
	look                          look
	shape                         shape
	metrics                       *metrics
}

func newLayer(name, version, node, instance string) (*layer, error) {
	versions, ok := recipes[name]
	if !ok {
		return nil, fmt.Errorf("unknown layer %q; the layers are %s", name, strings.Join(layerOrder, ", "))
	}
	look, ok := versions[version]
	if !ok {
		return nil, fmt.Errorf("%s has no version %q; it has v1, v2 and v3", name, version)
	}
	return &layer{name, version, node, instance, look, shapes[name], newMetrics()}, nil
}

func (l *layer) slice() Slice {
	return Slice{
		Layer:    l.name,
		Version:  l.version,
		Style:    l.look.Style,
		Node:     l.node,
		Instance: l.instance,
		Text:     l.text(),
		SVG:      fmt.Sprintf(`<g class="%s">%s</g>`, l.name, l.shape.draw(l.look)),
		Height:   l.shape.Height,
	}
}

func (l *layer) text() string {
	if !l.shape.Filled {
		return fg(l.look.Background) + "\x1b[1m" + l.shape.Left + l.look.Pattern + l.shape.Right + reset
	}
	return fg(l.look.Background) + l.shape.Left +
		bg(l.look.Background) + fg(l.look.Foreground) + l.look.Pattern + reset +
		fg(l.look.Background) + l.shape.Right + reset
}

func (l *layer) routes() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /layer", func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(l.slice())
	})
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, _ *http.Request) {
		if l.look.Broken {
			http.Error(w, fmt.Sprintf("%s %s is %s in the middle", l.name, l.version, l.look.Style), http.StatusServiceUnavailable)
			return
		}
		fmt.Fprintln(w, "ok")
	})
	mux.HandleFunc("GET /metrics", l.metrics.serve)
	return l.metrics.wrap(mux)
}

const reset = "\x1b[0m"

func fg(colour int) string { return fmt.Sprintf("\x1b[38;5;%dm", colour) }
func bg(colour int) string { return fmt.Sprintf("\x1b[48;5;%dm", colour) }

// The SVG artwork: flat shapes in a 260-wide box, one function per layer.

func drawTopBun(l look) string {
	var seeds strings.Builder
	if l.Style != "brioche" {
		for i, x := range []int{70, 100, 130, 160, 190, 85, 115, 145, 175} {
			y := 22
			if i >= 5 {
				y = 36
			}
			fmt.Fprintf(&seeds, `<ellipse cx="%d" cy="%d" rx="5" ry="2.5" fill="%s"/>`, x, y, l.Accent)
		}
	} else {
		fmt.Fprintf(&seeds, `<path d="M60 22 Q130 4 200 22" stroke="%s" stroke-width="5" fill="none" stroke-linecap="round"/>`, l.Accent)
	}
	return fmt.Sprintf(`<path d="M8 56 Q8 2 130 2 Q252 2 252 56 Z" fill="%s"/>%s`, l.Fill, seeds.String())
}

func drawLettuce(l look) string {
	var path strings.Builder
	path.WriteString("M2 6")
	for x := 2; x < 258; x += 16 {
		fmt.Fprintf(&path, " Q%d 16 %d 6", x+8, x+16)
	}
	path.WriteString(" L258 0 L2 0 Z")
	return fmt.Sprintf(`<path d="%s" fill="%s" stroke="%s" stroke-width="1.5"/>`, path.String(), l.Fill, l.Accent)
}

func drawTomato(l look) string {
	var seeds strings.Builder
	for x := 40; x <= 220; x += 36 {
		fmt.Fprintf(&seeds, `<ellipse cx="%d" cy="10" rx="6" ry="3" fill="%s"/>`, x, l.Accent)
	}
	return fmt.Sprintf(`<rect x="14" y="1" width="232" height="18" rx="9" fill="%s"/>%s`, l.Fill, seeds.String())
}

func drawCheese(l look) string {
	holes := ""
	if l.Style == "blue cheese" {
		for _, x := range []int{30, 90, 150, 220} {
			holes += fmt.Sprintf(`<circle cx="%d" cy="5" r="2.5" fill="%s"/>`, x, l.Accent)
		}
	}
	// One closed outline: along the top, then back along the bottom edge
	// from right to left, down and up through each drip.
	return fmt.Sprintf(`<path d="M4 0 H256 V10 H206 L198 22 L190 10 H136 L128 22 L120 10 H66 L58 22 L50 10 H4 Z" fill="%s" stroke="%s" stroke-width="1"/>%s`,
		l.Fill, l.Accent, holes)
}

func drawPatty(l look) string {
	var grill strings.Builder
	for x := 40; x <= 220; x += 30 {
		fmt.Fprintf(&grill, `<path d="M%d 8 L%d 22" stroke="%s" stroke-width="3" stroke-linecap="round"/>`, x, x+12, l.Accent)
	}
	return fmt.Sprintf(`<rect x="6" y="1" width="248" height="28" rx="14" fill="%s"/>%s`, l.Fill, grill.String())
}

func drawBottomBun(l look) string {
	return fmt.Sprintf(`<path d="M8 0 H252 Q254 26 222 26 H38 Q6 26 8 0 Z" fill="%s" stroke="%s" stroke-width="1"/>`, l.Fill, l.Accent)
}
