//! The Python proxy that gives a task container a PTY.
//!
//! `docker exec` has no terminal unless `-t` is requested, and `-t` fails
//! when the host side is not a terminal. The proxy therefore allocates a PTY
//! *inside* the container with [`pty.fork`] and forwards bytes between that
//! PTY and the plain `docker exec` stdin/stdout pipes.
//!
//! [`pty.spawn`] is deliberately not used: it calls `termios` on the
//! non-TTY stdin and can fail with `ENOTTY`. The explicit `select` loop works
//! with piped stdin.
//!
//! The proxy writes the shell PID, not its own PID, to the pidfile. `pty.fork`
//! makes the child a session leader, so the pid is also the process-group id
//! that close signals with `kill -TERM -<pid>`.
//!
//! After the pidfile is written the proxy prints [`READY_MARKER`] on stdout.
//! [`crate::session::PtySessions::open`] waits for that marker before it
//! returns a session id, so a missing `python3`, a bad `cwd`, or a missing
//! `/bin/sh` is reported as an open error instead of a dead session.

/// The line the proxy prints after the PTY exists and the pidfile is written.
///
/// The leading and trailing `\x1e` make a collision with shell output
/// extremely unlikely. The marker is stripped from the session buffer.
pub(crate) const READY_MARKER: &[u8] = b"\x1eDHI_PTY_READY\x1e\n";

/// The proxy run as `python3 -u -c <this> <cwd> <pidfile>`.
///
/// `cwd` is the resolved directory inside the container. `pidfile` receives
/// the shell PID after `pty.fork`. The proxy starts no process before a
/// failed `chdir` or pidfile write has been reported on stderr.
pub(crate) const PYTHON_PROXY: &str = r#"import os, pty, select, signal, sys

READY = b"\x1eDHI_PTY_READY\x1e\n"

def write_all(fd, data):
    view = memoryview(data)
    while len(view):
        try:
            written = os.write(fd, view)
        except InterruptedError:
            continue
        if written <= 0:
            raise OSError("write returned no bytes")
        view = view[written:]

def kill_group(pid):
    if pid is None or pid <= 1:
        return
    try:
        os.killpg(pid, signal.SIGKILL)
    except OSError:
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass

cwd = sys.argv[1] if len(sys.argv) > 1 else ""
pidfile = sys.argv[2] if len(sys.argv) > 2 else ""
if cwd:
    os.chdir(cwd)
if not os.path.isfile("/bin/sh") or not os.access("/bin/sh", os.X_OK):
    raise SystemExit("dhi-pty: /bin/sh is missing or not executable")

pid, fd = pty.fork()
if pid == 0:
    try:
        # `+m` disables job control. Without it an interactive shell puts a
        # background or foreground job in its own process group, and a close
        # that signals the shell's group would leave the job running.
        os.execv("/bin/sh", ["/bin/sh", "+m"])
    finally:
        os._exit(127)

try:
    directory = os.path.dirname(pidfile)
    if directory:
        os.makedirs(directory, exist_ok=True)
    with open(pidfile, "w") as handle:
        handle.write(str(pid))
    write_all(1, READY)
    while True:
        try:
            readable, _, _ = select.select([fd, 0], [], [])
        except OSError:
            break
        if fd in readable:
            try:
                data = os.read(fd, 65536)
            except OSError:
                break
            if not data:
                break
            try:
                write_all(1, data)
            except OSError:
                break
        if 0 in readable:
            try:
                data = os.read(0, 65536)
            except OSError:
                break
            if not data:
                break
            try:
                write_all(fd, data)
            except OSError:
                break
finally:
    kill_group(pid)
"#;
