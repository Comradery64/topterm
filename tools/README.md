# tools

Regenerating `assets/demo.svg` after a visual change:

```sh
cargo build --release
python3 tools/capture_frame.py 140 36 /tmp/frame.ans ./target/release/topterm --fake-data --demo-frame
python3 tools/ansi2svg.py /tmp/frame.ans assets/demo.svg "topterm — ttys004 leaking"
```

And the plain-text block in the top-level README:

```sh
COLUMNS=112 ./target/release/topterm --fake-data --no-color
```

`--fake-data` renders the synthetic machine in `src/fake.rs`, so nothing from
the machine you run this on ends up in the repo. Both scripts are stdlib-only.
