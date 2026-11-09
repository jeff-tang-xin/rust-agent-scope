// Package store keeps notes in memory with optimistic versioning.
package store

import (
	"errors"
	"fmt"
	"sort"
	"strings"
	"sync"
	"time"
)

var (
	ErrNotFound = errors.New("note not found")
	ErrConflict = errors.New("version conflict")
)

type Note struct {
	ID      int
	Title   string
	Body    string
	Version int
	Updated time.Time
}

type Store struct {
	mu     sync.RWMutex
	nextID int
	notes  map[int]*Note
	now    func() time.Time
}

func New() *Store {
	return &Store{nextID: 1, notes: map[int]*Note{}, now: time.Now}
}

func (s *Store) Create(title, body string) (*Note, error) {
	title = strings.TrimSpace(title)
	if title == "" {
		return nil, fmt.Errorf("create: %w", errors.New("title is required"))
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	n := &Note{ID: s.nextID, Title: title, Body: body, Version: 1, Updated: s.now()}
	s.notes[n.ID] = n
	s.nextID++
	return n, nil
}

func (s *Store) Get(id int) (Note, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	n, ok := s.notes[id]
	if !ok {
		return Note{}, ErrNotFound
	}
	return *n, nil
}

// Update replaces the body when version matches the stored one.
func (s *Store) Update(id, version int, body string) (Note, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	n, ok := s.notes[id]
	if !ok {
		return Note{}, ErrNotFound
	}
	if n.Version != version {
		return Note{}, ErrConflict
	}
	n.Body = body
	n.Version++
	n.Updated = s.now()
	return *n, nil
}

// TODO: full-text search instead of a substring match
func (s *Store) Search(q string) []Note {
	s.mu.RLock()
	defer s.mu.RUnlock()
	var out []Note
	for _, n := range s.notes {
		if strings.Contains(strings.ToLower(n.Title+" "+n.Body), strings.ToLower(q)) {
			out = append(out, *n)
		}
	}
	sort.Slice(out, func(i, j int) bool { return out[i].ID < out[j].ID })
	return out
}

func (s *Store) debugString() string {
	fmt.Sprintf("%d notes", len(s.notes)) // intentional warning: result of fmt.Sprintf not used
	return "store"
}
