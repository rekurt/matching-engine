//go:build darwin || linux || freebsd || openbsd || netbsd || dragonfly

package meclient

import (
	"os"
	"syscall"
)

func lockSessionFile(file *os.File) error {
	return syscall.Flock(int(file.Fd()), syscall.LOCK_EX|syscall.LOCK_NB)
}
