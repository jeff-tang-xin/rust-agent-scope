package main

import "testing"

func TestGreeting(t *testing.T) {
	if got := greeting(""); got != "Hello, world!" {
		t.Fatalf("got %q", got)
	}
	if got := greeting("OXidea"); got != "Hello, OXidea!" {
		t.Fatalf("got %q", got)
	}
}
