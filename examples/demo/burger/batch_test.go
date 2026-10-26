package main

import (
	"bytes"
	"testing"
)

func TestBatchRetryHasStableResultAndIndexesDiffer(t *testing.T) {
	var first, retry, next bytes.Buffer
	for _, test := range []struct {
		index  string
		output *bytes.Buffer
	}{{"42", &first}, {"42", &retry}, {"43", &next}} {
		if err := batch([]string{"small", test.index}, test.output); err != nil {
			t.Fatal(err)
		}
	}
	if !bytes.Equal(first.Bytes(), retry.Bytes()) {
		t.Fatal("retry changed result")
	}
	if bytes.Equal(first.Bytes(), next.Bytes()) {
		t.Fatal("different task identities shared a result")
	}
}
func TestACookedPattyIsReportedToTheCounter(t *testing.T) {
	c := testCounter(wholeKitchen(t))
	k := newKitchen(t)
	k.serve("counter", c.routes())
	if err := reportCooked(k.url("counter"), "7/small/3"); err != nil {
		t.Fatal(err)
	}
	if c.ownTally() != 1 {
		t.Errorf("the counter has %d patties", c.ownTally())
	}
	k.remove("counter")
	if err := reportCooked(k.url("counter"), "7/small/4"); err == nil {
		t.Error("a report nobody took succeeded")
	}
}

func TestBatchRejectsInvalidWork(t *testing.T) {
	for _, args := range [][]string{nil, {"huge", "0"}, {"small", "-1"}, {"large", "4294967296"}} {
		if err := batch(args, &bytes.Buffer{}); err == nil {
			t.Fatalf("accepted %v", args)
		}
	}
}
