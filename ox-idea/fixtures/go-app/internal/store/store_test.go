package store

import (
	"errors"
	"os"
	"testing"
)

func TestCreateAndGet(t *testing.T) {
	s := New()
	n, err := s.Create("  Groceries ", "milk")
	if err != nil {
		t.Fatal(err)
	}
	got, err := s.Get(n.ID)
	if err != nil || got.Title != "Groceries" {
		t.Fatalf("got %+v, %v", got, err)
	}
}

func TestUpdateConflict(t *testing.T) {
	s := New()
	n, _ := s.Create("a", "")
	if _, err := s.Update(n.ID, 1, "b"); err != nil {
		t.Fatal(err)
	}
	if _, err := s.Update(n.ID, 1, "c"); !errors.Is(err, ErrConflict) {
		t.Fatalf("want conflict, got %v", err)
	}
}

func TestSearch(t *testing.T) {
	s := New()
	s.Create("Shopping", "eggs")
	s.Create("Work", "review PR")
	s.Create("Weekend", "buy EGGS")
	if got := s.Search("eggs"); len(got) != 2 {
		t.Fatalf("want 2 results, got %d", len(got))
	}
}

func TestIntentionallyFailing(t *testing.T) {
	if os.Getenv("OPENIDEA_FAIL") == "" {
		t.Skip("set OPENIDEA_FAIL=1 to run")
	}
	// FIXME: wrong on purpose (QA: failing test in the test tree)
	if len(New().Search("")) != 1 {
		t.Error("expected one note")
	}
}
