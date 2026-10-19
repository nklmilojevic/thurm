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
		cred, err := unix.GetsockoptXucred(int(fd), unix.SOL_LOCAL, unix.LOCAL_PEERCRED)
		ok = err == nil && (cred.Uid == uint32(os.Getuid()) || cred.Uid == 0)
	})
	return ok
}
