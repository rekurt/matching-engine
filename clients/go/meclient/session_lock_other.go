//go:build !darwin && !linux && !freebsd && !openbsd && !netbsd && !dragonfly

package meclient

import (
	"errors"
	"os"
)

func lockSessionFile(file *os.File) error {
	return errors.New("file session locking is unavailable on this platform; supply a durable SessionStore")
}
