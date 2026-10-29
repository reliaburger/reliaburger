package main

import (
	"encoding/json"
	"net/http"
	"regexp"
	"strings"
	"testing"
)

var ansi = regexp.MustCompile("\x1b\\[[0-9;]*m")

func TestCurlGetsAColouredBurgerOneLinePerLayer(t *testing.T) {
	c := testCounter(wholeKitchen(t))
	for _, accept := range []string{"", "*/*", "text/plain"} {
		response := get(t, c.routes(), "/", accept)
		if response.Code != http.StatusOK {
			t.Fatalf("status %d", response.Code)
		}
		if got := response.Header().Get("Content-Type"); !strings.HasPrefix(got, "text/plain") {
			t.Errorf("Accept %q got %q", accept, got)
		}
		body := response.Body.String()
		if !strings.Contains(body, "\x1b[48;5;") {
			t.Errorf("no colour in %q", body)
		}
		plain := ansi.ReplaceAllString(body, "")
		for _, want := range []string{
			"top bun     v1  sesame",
			"cheese      v1  cheddar",
			"bottom bun  v1",
			"rb-test-1 #i-patty",
			"counter rb-test-2 #c0de",
			"patties cooked: 0",
		} {
			if !strings.Contains(plain, want) {
				t.Errorf("Accept %q: no %q in\n%s", accept, want, plain)
			}
		}
		layers := 0
		for _, line := range strings.Split(plain, "\n") {
			for _, name := range layerOrder {
				if strings.Contains(line, " "+strings.ReplaceAll(name, "-", " ")+" ") {
					layers++
				}
			}
		}
		if layers != len(layerOrder) {
			t.Errorf("%d layer lines in\n%s", layers, plain)
		}
	}
}

func TestTheTerminalBurgerShowsAGapAndWhy(t *testing.T) {
	k := wholeKitchen(t)
	k.remove("lettuce")
	plain := ansi.ReplaceAllString(get(t, testCounter(k).routes(), "/", "").Body.String(), "")
	lines := strings.Split(plain, "\n")
	var gap string
	for _, line := range lines {
		if strings.Contains(line, "lettuce") {
			gap = line
		}
	}
	if !strings.Contains(gap, "· ") || !strings.Contains(gap, "missing: not answering") {
		t.Errorf("lettuce line %q", gap)
	}
}

func TestABrowserGetsSVGLayersThatRefresh(t *testing.T) {
	k := wholeKitchen(t)
	k.remove("cheese")
	response := get(t, testCounter(k).routes(), "/", "text/html,application/xhtml+xml,*/*;q=0.8")
	if got := response.Header().Get("Content-Type"); !strings.HasPrefix(got, "text/html") {
		t.Fatalf("Content-Type %q", got)
	}
	page := response.Body.String()
	for _, want := range []string{"<svg", recipes["patty"]["v1"].Fill, "setInterval", "/burger.svg", "cheese missing"} {
		if !strings.Contains(page, want) {
			t.Errorf("the page has no %q", want)
		}
	}
	if strings.Contains(page, "\x1b") {
		t.Error("the page has terminal escapes")
	}

	svg := get(t, testCounter(k).routes(), "/burger.svg", "")
	if got := svg.Header().Get("Content-Type"); got != "image/svg+xml" {
		t.Errorf("Content-Type %q", got)
	}
	if body := svg.Body.String(); !strings.HasPrefix(body, "<svg") || strings.Count(body, "<g ") < len(layerOrder) {
		t.Errorf("svg %q", body)
	}
}

func TestScriptsGetJSON(t *testing.T) {
	k := wholeKitchen(t)
	k.remove("tomato")
	response := get(t, testCounter(k).routes(), "/", "application/json")
	if got := response.Header().Get("Content-Type"); got != "application/json" {
		t.Fatalf("Content-Type %q", got)
	}
	var burger struct {
		Counter  string `json:"counter"`
		Complete bool   `json:"complete"`
		Layers   []struct {
			Layer, Version, Style, Node, Instance, Missing string
			LatencyMS                                      *float64 `json:"latency_ms"`
		} `json:"layers"`
		Cooked *uint64 `json:"patties_cooked"`
	}
	if err := json.Unmarshal(response.Body.Bytes(), &burger); err != nil {
		t.Fatal(err)
	}
	if burger.Counter != "rb-test-2 #c0de" || burger.Complete || burger.Cooked == nil || len(burger.Layers) != len(layerOrder) {
		t.Fatalf("%+v", burger)
	}
	if top := burger.Layers[0]; top.Layer != "top-bun" || top.Version != "v1" || top.Style != "sesame" || top.LatencyMS == nil {
		t.Errorf("top bun %+v", top)
	}
	if tomato := burger.Layers[2]; tomato.Missing != "not answering" || tomato.Version != "" {
		t.Errorf("tomato %+v", tomato)
	}
	if strings.Contains(response.Body.String(), "\\u001b") || strings.Contains(response.Body.String(), "<g") {
		t.Error("the JSON carries the drawings")
	}
}
