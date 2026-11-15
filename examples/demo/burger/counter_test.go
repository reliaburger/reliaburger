package main

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"
)

// kitchen runs a fake service per name, so a test counter can find layers,
// and its peers, by service name as it does on the cluster.
type kitchen struct {
	t        *testing.T
	mu       sync.Mutex
	services map[string]*httptest.Server
}

func newKitchen(t *testing.T) *kitchen {
	k := &kitchen{t: t, services: map[string]*httptest.Server{}}
	t.Cleanup(func() {
		for _, server := range k.services {
			server.Close()
		}
	})
	return k
}

func (k *kitchen) serve(name string, handler http.Handler) {
	k.mu.Lock()
	defer k.mu.Unlock()
	k.services[name] = httptest.NewServer(handler)
}

// add starts a layer instance under its service name.
func (k *kitchen) add(name, version, instance string) {
	l, err := newLayer(name, version, "rb-test-1", instance)
	if err != nil {
		k.t.Fatal(err)
	}
	k.serve(name, l.routes())
}

// remove stops a service; connecting to it is then refused.
func (k *kitchen) remove(name string) {
	k.mu.Lock()
	defer k.mu.Unlock()
	if server, ok := k.services[name]; ok {
		server.Close()
	}
}

func (k *kitchen) url(service string) string {
	k.mu.Lock()
	defer k.mu.Unlock()
	if server, ok := k.services[service]; ok {
		return server.URL
	}
	return "http://127.0.0.1:1"
}

func wholeKitchen(t *testing.T) *kitchen {
	k := newKitchen(t)
	for _, name := range layerOrder {
		k.add(name, "v1", "i-"+name)
	}
	return k
}

func testCounter(k *kitchen) *counter {
	c := newCounter("rb-test-2", "c0de", k.url)
	c.probes = 2
	return c
}

func TestTheCounterStacksEveryLayerTopToBottom(t *testing.T) {
	burger := testCounter(wholeKitchen(t)).assemble(context.Background())
	if !burger.Complete {
		t.Errorf("a whole kitchen made an incomplete burger: %+v", burger)
	}
	var names []string
	for _, row := range burger.Rows {
		names = append(names, row.Layer)
		if row.Missing != "" || row.Version != "v1" || row.Instance != "i-"+row.Layer {
			t.Errorf("row %+v", row)
		}
	}
	if strings.Join(names, " ") != strings.Join(layerOrder, " ") {
		t.Errorf("stacked %v", names)
	}
}

func TestAMissingLayerIsAGapInItsPlace(t *testing.T) {
	k := wholeKitchen(t)
	k.remove("lettuce")
	burger := testCounter(k).assemble(context.Background())
	if burger.Complete {
		t.Error("a burger without lettuce is complete")
	}
	if len(burger.Rows) != len(layerOrder) {
		t.Fatalf("%d rows, want %d", len(burger.Rows), len(layerOrder))
	}
	gap := burger.Rows[1]
	if gap.Layer != "lettuce" || gap.Missing == "" || gap.Version != "" {
		t.Errorf("row 2 is %+v, want a lettuce gap", gap)
	}
	if burger.Rows[2].Layer != "tomato" || burger.Rows[2].Missing != "" {
		t.Errorf("the tomato moved: %+v", burger.Rows[2])
	}
}

func TestAnUnhealthyAnswerIsAGap(t *testing.T) {
	k := wholeKitchen(t)
	k.serve("tomato", http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		http.Error(w, "squashed", http.StatusInternalServerError)
	}))
	burger := testCounter(k).assemble(context.Background())
	if row := burger.Rows[2]; row.Missing != "answered 500" {
		t.Errorf("tomato row %+v", row)
	}
}

func TestASlowLayerPastTheTimeoutIsAGap(t *testing.T) {
	k := wholeKitchen(t)
	release := make(chan struct{})
	t.Cleanup(func() { close(release) })
	k.serve("patty", http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		select {
		case <-release:
		case <-r.Context().Done():
		}
	}))
	c := testCounter(k)
	c.timeout = 50 * time.Millisecond
	if row := c.assemble(context.Background()).Rows[4]; row.Missing != "timed out" {
		t.Errorf("patty row %+v", row)
	}
}

func TestEachPattyInstanceIsAPatty(t *testing.T) {
	k := wholeKitchen(t)
	instances := []string{"p1", "p2", "p3"}
	var mu sync.Mutex
	next := 0
	patties := map[string]http.Handler{}
	for _, instance := range instances {
		l, err := newLayer("patty", "v1", "rb-test-1", instance)
		if err != nil {
			t.Fatal(err)
		}
		patties[instance] = l.routes()
	}
	// Round-robin, like the eBPF service map: each connection gets the next instance.
	k.serve("patty", http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		instance := instances[next%len(instances)]
		next++
		mu.Unlock()
		patties[instance].ServeHTTP(w, r)
	}))
	c := testCounter(k)
	c.probes = 3
	burger := c.assemble(context.Background())
	var got []string
	for _, row := range burger.Rows {
		if row.Layer == "patty" {
			got = append(got, row.Instance)
		}
	}
	if strings.Join(got, " ") != "p1 p2 p3" {
		t.Errorf("patties %v, want a triple patty in instance order", got)
	}
	if len(burger.Rows) != len(layerOrder)+2 {
		t.Errorf("%d rows", len(burger.Rows))
	}
}

func TestAnInstanceSeenRecentlyStaysUntilItsLayerDisappears(t *testing.T) {
	k := wholeKitchen(t)
	now := time.Unix(1000, 0)
	c := testCounter(k)
	c.now = func() time.Time { return now }
	c.assemble(context.Background())

	// A second cheese instance answers a second later: both are on the
	// burger, in node and instance order, so every counter draws the same.
	now = now.Add(time.Second)
	k.add("cheese", "v2", "c2")
	burger := c.assemble(context.Background())
	if cheeses := rowsOf(burger, "cheese"); cheeses != "c2/v2 i-cheese/v1" {
		t.Errorf("cheese rows %q", cheeses)
	}
	// Once the old one hasn't answered for longer than the memory, it goes.
	now = now.Add(c.remember + time.Second)
	if cheeses := rowsOf(c.assemble(context.Background()), "cheese"); cheeses != "c2/v2" {
		t.Errorf("cheese rows %q", cheeses)
	}
	// A layer nobody answers for is a gap at once, whatever was seen before.
	k.remove("cheese")
	if cheeses := rowsOf(c.assemble(context.Background()), "cheese"); cheeses != "/" {
		t.Errorf("cheese rows %q", cheeses)
	}
}

func rowsOf(burger Burger, layer string) string {
	var rows []string
	for _, row := range burger.Rows {
		if row.Layer == layer {
			rows = append(rows, row.Instance+"/"+row.Version)
		}
	}
	return strings.Join(rows, " ")
}

func TestCookedPattiesAreCountedOnceAndSummedAcrossCounters(t *testing.T) {
	k := wholeKitchen(t)
	other := testCounter(k)
	other.instance = "beef"
	k.serve("counter", http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		other.routes().ServeHTTP(w, r)
	}))
	for _, key := range []string{"1/small/0", "1/small/1", "1/small/1"} {
		response := post(t, other.routes(), "/cooked", `{"patty":"`+key+`"}`)
		if response.Code != http.StatusOK {
			t.Fatalf("status %d: %s", response.Code, response.Body)
		}
	}
	c := testCounter(k)
	post(t, c.routes(), "/cooked", `{"patty":"1/large/0"}`)
	// This counter asks the counter service; in the test only `other` answers
	// there, so it adds its own tally to what `other` reports.
	if burger := c.assemble(context.Background()); burger.Cooked != 3 {
		t.Errorf("cooked %d, want 3 (two from the other counter, one here)", burger.Cooked)
	}
}

func TestACookedReportNeedsAPatty(t *testing.T) {
	c := testCounter(wholeKitchen(t))
	for _, body := range []string{`{}`, `not json`} {
		if response := post(t, c.routes(), "/cooked", body); response.Code != http.StatusBadRequest {
			t.Errorf("%s: status %d", body, response.Code)
		}
	}
}

func post(t *testing.T, handler http.Handler, path, body string) *httptest.ResponseRecorder {
	t.Helper()
	recorder := httptest.NewRecorder()
	handler.ServeHTTP(recorder, httptest.NewRequest(http.MethodPost, path, strings.NewReader(body)))
	return recorder
}

func TestAnOrderNamesItsBurgerAndWhatWasMissing(t *testing.T) {
	k := wholeKitchen(t)
	c := testCounter(k)
	for number, name := range []string{"cheeseburger", "double smash"} {
		response := get(t, c.routes(), "/order", "")
		var order Order
		if err := json.Unmarshal(response.Body.Bytes(), &order); err != nil {
			t.Fatal(err)
		}
		if order.Number != uint64(number+1) || order.Burger != name || order.Counter != "rb-test-2 #c0de" || !order.Complete {
			t.Errorf("order %+v", order)
		}
	}
	k.remove("tomato")
	var order Order
	json.Unmarshal(get(t, c.routes(), "/order", "").Body.Bytes(), &order)
	if order.Complete || strings.Join(order.Missing, ",") != "tomato" {
		t.Errorf("order without tomato %+v", order)
	}
}

func TestTheCounterIsHealthyWithoutItsLayers(t *testing.T) {
	c := testCounter(newKitchen(t))
	if response := get(t, c.routes(), "/healthz", ""); response.Code != http.StatusOK {
		t.Errorf("status %d", response.Code)
	}
}

func TestTheDinerOrdersFromTheCounter(t *testing.T) {
	c := testCounter(wholeKitchen(t))
	k := newKitchen(t)
	k.serve("counter", c.routes())
	if err := orderOnce(k.url("counter")); err != nil {
		t.Fatal(err)
	}
	if c.orders.Load() != 1 {
		t.Errorf("the counter took %d orders", c.orders.Load())
	}
	k.remove("counter")
	if err := orderOnce(k.url("counter")); err == nil {
		t.Error("an order nobody took succeeded")
	}
}
