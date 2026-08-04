"""Run a command on a pty of a fixed size and dump its raw output."""
import fcntl, os, pty, struct, subprocess, sys, termios

cols, rows, out_path = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
cmd = sys.argv[4:]

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
env = dict(os.environ, COLORTERM="truecolor", TERM="xterm-256color", COLUMNS=str(cols))
proc = subprocess.Popen(cmd, stdin=slave, stdout=slave, stderr=slave, env=env,
                        start_new_session=True)
os.close(slave)
chunks = []
while True:
    try:
        data = os.read(master, 65536)
    except OSError:
        break
    if not data:
        break
    chunks.append(data)
proc.wait()
with open(out_path, "wb") as f:
    f.write(b"".join(chunks))
print(f"{len(b''.join(chunks))} bytes -> {out_path}")
