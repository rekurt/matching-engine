package meclient

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sync"
)

const SessionStorageVersion = 1

type PendingCommand struct {
	RequestID uint64 `json:"request_id"`
	Sequence  uint64 `json:"sequence"`
	Frame     []byte `json:"frame"`
}

// SessionState is one atomic checkpoint. Application contains the consumer's state
// at AppliedSequence; external database stores must commit both in one transaction.
type SessionState struct {
	Version                uint32           `json:"version"`
	Endpoint               string           `json:"endpoint"`
	SessionID              []byte           `json:"session_id"`
	AppliedSequence        uint64           `json:"applied_sequence"`
	NextClientSequence     uint64           `json:"next_client_sequence"`
	NextRequestID          uint64           `json:"next_request_id"`
	Pending                []PendingCommand `json:"pending"`
	Application            json.RawMessage  `json:"application"`
	ReconciliationRequired bool             `json:"reconciliation_required"`
	ReconciliationReason   string           `json:"reconciliation_reason,omitempty"`
}

func FreshSessionState() SessionState {
	return SessionState{Version: SessionStorageVersion, NextClientSequence: 2, NextRequestID: 1}
}

// SessionStore must have one owner and durable, atomic Save semantics. Load must
// report corruption or missing existing state rather than return a fresh session.
type SessionStore interface {
	Load(context.Context) (SessionState, error)
	Save(context.Context, SessionState) error
}
type FileSessionStore struct {
	mu           sync.Mutex
	path         string
	lock         *os.File
	closed       bool
	beforeRename func() error
}
type sessionFile struct {
	Checksum string          `json:"checksum"`
	State    json.RawMessage `json:"state"`
}

// OpenFileSessionStore creates a new checkpoint only when both state and lock files
// are absent. An interrupted initialization or deleted state requires reconciliation.
func OpenFileSessionStore(path string) (*FileSessionStore, error) {
	path, err := filepath.Abs(path)
	if err != nil {
		return nil, err
	}
	if err = os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		return nil, err
	}
	lockInfo, lockErr := os.Lstat(path + ".lock")
	if lockErr == nil && !lockInfo.Mode().IsRegular() {
		return nil, errors.New("session lock must be a regular file")
	}
	lock, err := os.OpenFile(path+".lock", os.O_CREATE|os.O_RDWR, 0600)
	if err != nil {
		return nil, err
	}
	if err = lock.Chmod(0600); err != nil {
		lock.Close()
		return nil, err
	}
	if err = lockSessionFile(lock); err != nil {
		lock.Close()
		return nil, err
	}
	store := &FileSessionStore{path: path, lock: lock}
	if info, err := os.Lstat(path); errors.Is(err, os.ErrNotExist) {
		if !errors.Is(lockErr, os.ErrNotExist) {
			store.Close()
			return nil, fmt.Errorf("session state is missing beside an existing lock: %w", ErrReconciliationRequired)
		}
		if err = store.Save(context.Background(), FreshSessionState()); err != nil {
			store.Close()
			return nil, err
		}
	} else if err != nil {
		store.Close()
		return nil, err
	} else if !info.Mode().IsRegular() {
		store.Close()
		return nil, errors.New("session state must be a regular file")
	}
	if _, err = store.Load(context.Background()); err != nil {
		store.Close()
		return nil, err
	}
	return store, nil
}
func (s *FileSessionStore) verifyLock() error {
	held, err := s.lock.Stat()
	if err != nil {
		return err
	}
	linked, err := os.Lstat(s.path + ".lock")
	if err != nil {
		return fmt.Errorf("session lock replaced: %w", ErrReconciliationRequired)
	}
	if !linked.Mode().IsRegular() || !os.SameFile(held, linked) {
		return fmt.Errorf("session lock replaced: %w", ErrReconciliationRequired)
	}
	return nil
}
func (s *FileSessionStore) Close() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return nil
	}
	s.closed = true
	return s.lock.Close()
}
func (s *FileSessionStore) Load(ctx context.Context) (SessionState, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	var state SessionState
	if err := s.verifyLock(); err != nil {
		return state, err
	}
	if s.closed {
		return state, os.ErrClosed
	}
	if err := ctx.Err(); err != nil {
		return state, err
	}
	file, err := os.Open(s.path)
	if err != nil {
		return state, err
	}
	defer file.Close()
	if err = file.Chmod(0600); err != nil {
		return state, err
	}
	var envelope sessionFile
	decoder := json.NewDecoder(io.LimitReader(file, 64<<20))
	decoder.DisallowUnknownFields()
	if err = decoder.Decode(&envelope); err != nil {
		return state, err
	}
	var extra interface{}
	if err = decoder.Decode(&extra); err != io.EOF {
		return state, errors.New("trailing session state data")
	}
	sum := sha256.Sum256(envelope.State)
	if hex.EncodeToString(sum[:]) != envelope.Checksum {
		return state, errors.New("session state checksum mismatch")
	}
	decoder = json.NewDecoder(bytesReader(envelope.State))
	decoder.DisallowUnknownFields()
	if err = decoder.Decode(&state); err != nil {
		return state, err
	}
	return state, validateSessionState(state)
}
func (s *FileSessionStore) Save(ctx context.Context, state SessionState) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return os.ErrClosed
	}
	if err := s.verifyLock(); err != nil {
		return err
	}
	if err := ctx.Err(); err != nil {
		return err
	}
	if err := validateSessionState(state); err != nil {
		return err
	}
	data, err := json.Marshal(state)
	if err != nil {
		return err
	}
	sum := sha256.Sum256(data)
	encoded, err := json.Marshal(sessionFile{hex.EncodeToString(sum[:]), data})
	if err != nil {
		return err
	}
	if len(encoded) > 64<<20 {
		return ErrFrameTooLarge
	}
	file, err := os.CreateTemp(filepath.Dir(s.path), ".session-*")
	if err != nil {
		return err
	}
	name := file.Name()
	defer os.Remove(name)
	defer file.Close()
	if err = file.Chmod(0600); err != nil {
		return err
	}
	if _, err = file.Write(encoded); err != nil {
		return err
	}
	if err = file.Sync(); err != nil {
		return err
	}
	if err = file.Close(); err != nil {
		return err
	}
	if s.beforeRename != nil {
		if err = s.beforeRename(); err != nil {
			return err
		}
	}
	if err = s.verifyLock(); err != nil {
		return err
	}
	if err = os.Rename(name, s.path); err != nil {
		return err
	}
	dir, err := os.Open(filepath.Dir(s.path))
	if err != nil {
		return err
	}
	defer dir.Close()
	return dir.Sync()
}
