package main

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func get(t *testing.T, handler http.Handler, path, accept string) *httptest.ResponseRecorder {
	t.Helper()
	request := httptest.NewRequest(http.MethodGet, path, nil)
	if accept != "" {
		request.Header.Set("Accept", accept)
	}
	recorder := httptest.NewRecorder()
	handler.ServeHTTP(recorder, request)
	return recorder
}

func mustLayer(t *testing.T, name, version string) *layer {
	t.Helper()
	l, err := newLayer(name, version, "rb-test-1", "a1b2")
	if err != nil {
		t.Fatal(err)
	}
	return l
}

func TestALayerServesItsSliceWithVersionAndInstance(t *testing.T) {
	response := get(t, mustLayer(t, "cheese", "v1").routes(), "/layer", "")
	if response.Code != http.StatusOK {
		t.Fatalf("status %d: %s", response.Code, response.Body)
	}
	var slice Slice
	if err := json.Unmarshal(response.Body.Bytes(), &slice); err != nil {
		t.Fatal(err)
	}
	if slice.Layer != "cheese" || slice.Version != "v1" || slice.Style != "cheddar" ||
		slice.Node != "rb-test-1" || slice.Instance != "a1b2" {
		t.Errorf("got %+v", slice)
	}
	if !strings.Contains(slice.Text, "\x1b[") {
		t.Errorf("the terminal line has no colour: %q", slice.Text)
	}
	if !strings.HasPrefix(slice.SVG, "<g") || slice.Height <= 0 {
		t.Errorf("the SVG slice is %q, %d high", slice.SVG, slice.Height)
	}
}

func TestEveryLayerHasThreeVersionsThatLookDifferent(t *testing.T) {
	for _, name := range layerOrder {
		seen := map[string]string{}
		for _, version := range []string{"v1", "v2", "v3"} {
			slice := mustLayer(t, name, version).slice()
			if other, ok := seen[slice.Text]; ok {
				t.Errorf("%s %s draws the same line as %s", name, version, other)
			}
			seen[slice.Text] = version
		}
	}
}

func TestCheeseV2IsADifferentColour(t *testing.T) {
	v1, v2 := recipes["cheese"]["v1"], recipes["cheese"]["v2"]
	if v1.Fill == v2.Fill || v1.Background == v2.Background {
		t.Errorf("cheese v1 and v2 share a colour: %+v %+v", v1, v2)
	}
	if !strings.Contains(mustLayer(t, "cheese", "v2").slice().SVG, v2.Fill) {
		t.Error("cheese v2's SVG doesn't use its own colour")
	}
}

func TestAHealthyLayerPassesItsHealthCheck(t *testing.T) {
	for _, name := range layerOrder {
		response := get(t, mustLayer(t, name, "v1").routes(), "/healthz", "")
		if response.Code != http.StatusOK {
			t.Errorf("%s v1 answered %d", name, response.Code)
		}
	}
}

func TestTheBrokenPattyFailsItsHealthCheck(t *testing.T) {
	response := get(t, mustLayer(t, "patty", "v3").routes(), "/healthz", "")
	if response.Code != http.StatusServiceUnavailable {
		t.Errorf("patty v3 answered %d, want 503", response.Code)
	}
	for _, version := range []string{"v1", "v2"} {
		if response := get(t, mustLayer(t, "patty", version).routes(), "/healthz", ""); response.Code != http.StatusOK {
			t.Errorf("patty %s answered %d", version, response.Code)
		}
	}
}

func TestAnUnknownLayerOrVersionIsRefused(t *testing.T) {
	for _, args := range [][2]string{{"pickle", "v1"}, {"cheese", "v4"}, {"", "v1"}} {
		if _, err := newLayer(args[0], args[1], "n", "i"); err == nil {
			t.Errorf("accepted %v", args)
		}
	}
}

func TestNodeNamesDropLimasPrefix(t *testing.T) {
	for hostname, want := range map[string]string{
		"lima-rb-05e5e50df86c-1": "rb-05e5e50df86c-1",
		"worker-7":               "worker-7",
	} {
		if got := nodeName(hostname); got != want {
			t.Errorf("nodeName(%q) = %q, want %q", hostname, got, want)
		}
	}
}

func TestEveryLineIsTheSameWidth(t *testing.T) {
	for _, name := range layerOrder {
		for version, look := range recipes[name] {
			if width := len([]rune(look.Pattern)); width != 24 {
				t.Errorf("%s %s's pattern is %d wide, want 24", name, version, width)
			}
		}
	}
	if width := len([]rune(ansi.ReplaceAllString(gapLine, ""))); width != 26 {
		t.Errorf("the gap is %d wide, want 26", width)
	}
}
