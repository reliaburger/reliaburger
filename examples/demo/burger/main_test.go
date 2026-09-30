package main

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"
)

func get(t *testing.T, handler http.Handler, path string) *httptest.ResponseRecorder {
	t.Helper()
	recorder := httptest.NewRecorder()
	handler.ServeHTTP(recorder, httptest.NewRequest(http.MethodGet, path, nil))
	return recorder
}

func TestOrderNamesTheKitchenThatCookedIt(t *testing.T) {
	backend := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Accept") != "application/json" {
			t.Errorf("podinfo answers HTML unless asked for JSON; got Accept %q", r.Header.Get("Accept"))
		}
		w.Write([]byte(`{"hostname":"backend-7f9c","version":"6.15.0"}`))
	}))
	defer backend.Close()
	handler := newShop(backend.URL, "burger-1").routes()

	for number, burger := range []string{"cheeseburger", "double smash"} {
		response := get(t, handler, "/order")
		if response.Code != http.StatusOK {
			t.Fatalf("status %d: %s", response.Code, response.Body)
		}
		var order Order
		if err := json.Unmarshal(response.Body.Bytes(), &order); err != nil {
			t.Fatal(err)
		}
		want := Order{Number: uint64(number + 1), Burger: burger, Cashier: "burger-1", Kitchen: "backend-7f9c"}
		if order != want {
			t.Errorf("got %+v, want %+v", order, want)
		}
	}
}

func TestOrderFailsWhenTheKitchenIsUnreachable(t *testing.T) {
	backend := httptest.NewServer(http.NotFoundHandler())
	backend.Close()
	response := get(t, newShop(backend.URL, "burger-1").routes(), "/order")
	if response.Code != http.StatusBadGateway {
		t.Errorf("status %d, want 502", response.Code)
	}
}

func TestHealthzAnswersWithoutTheKitchen(t *testing.T) {
	response := get(t, newShop("http://127.0.0.1:1/", "burger-1").routes(), "/healthz")
	if response.Code != http.StatusOK || response.Body.String() != "ok\n" {
		t.Errorf("got %d %q", response.Code, response.Body)
	}
}
