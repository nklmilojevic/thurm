package dial

import (
	"net"
	"os"

	"golang.org/x/sys/unix"
)

// SameUser reports whether the process on the other end of c runs as this user or root.
func SameUser(c *net.UnixConn) bool {
	raw, err := c.SyscallConn()
	if err != nil {
		return false
	}
	ok := false
	_ = raw.Control(func(fd uintptr) {
		cred, err := unix.GetsockoptUcred(int(fd), unix.SOL_SOCKET, unix.SO_PEERCRED)
		ok = err == nil && (cred.Uid == uint32(os.Getuid()) || cred.Uid == 0)
	})
	return ok
}
