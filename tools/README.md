# tools

Regenerating `assets/demo.svg` after a visual change:

```sh
cargo build --release
python3 tools/capture_frame.py 140 36 /tmp/frame.ans ./target/release/topterm --fake-data --demo-frame
python3 tools/ansi2svg.py /tmp/frame.ans assets/demo.svg "topterm — ttys004 leaking"
```

The other screenshots (all from `--fake-data`, never from your machine):

```sh
T=./target/release/topterm
python3 tools/capture_frame.py 120 40 /tmp/h.ans $T --fake-data --demo-hibernate ttys026
python3 tools/ansi2svg.py /tmp/h.ans assets/hibernate.svg "topterm — H: hibernate preview"
python3 tools/capture_frame.py 120 34 /tmp/z.ans $T --fake-data --demo-bulk
python3 tools/ansi2svg.py /tmp/z.ans assets/sleep-stale.svg "topterm — Z: hibernate sessions idle 7+ days"
python3 tools/capture_frame.py 104 24 /tmp/c.ans $T --fake-data --demo-card ttys026
python3 tools/ansi2svg.py /tmp/c.ans assets/card.svg "topterm — the card left in a hibernated tab"
```

And the plain-text block in the top-level README:

```sh
COLUMNS=112 ./target/release/topterm --fake-data --no-color
```

`--fake-data` renders the synthetic machine in `src/fake.rs`, so nothing from
the machine you run this on ends up in the repo. Both scripts are stdlib-only.
