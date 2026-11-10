"""Convert a captured ANSI terminal frame into a self-contained SVG.

Handles the SGR subset topterm's `buffer_to_ansi` emits: reset, bold, dim,
reverse, default fg/bg (39/49), 24-bit (38/48;2;r;g;b), 256-colour
(38/48;5;n) and the eight named colours. Cells are merged into runs of
identical style so the output stays small.
"""
import re
import sys
from xml.sax.saxutils import escape

CELL_W = 7.8
CELL_H = 14.0
FONT_SIZE = 12.5
BASELINE = 10.8          # text baseline inside a cell
PAD_X, PAD_TOP = 14.0, 12.0
TITLEBAR = 30.0
RADIUS = 10.0

DEFAULT_FG = "#d7dce5"
DEFAULT_BG = "#0b0e14"
PAGE_BG = "#05070a"
CHROME_BG = "#11151f"
CHROME_LINE = "#262e40"

NAMED = ["#0b0e14", "#f0442c", "#2dd4a7", "#f5a524", "#4c8bf5",
         "#b072e8", "#37c7d4", "#6b7488"]

FONT_STACK = ("'SF Mono','SFMono-Regular',Menlo,Monaco,'Cascadia Mono',"
              "'DejaVu Sans Mono','Liberation Mono',monospace")

SGR_RE = re.compile(r"\x1b\[([0-9;]*)m")


def xterm256(n):
    if n < 8:
        return NAMED[n]
    if n < 16:
        r, g, b = ((n & 1) and 255 or 85, (n & 2) and 255 or 85, (n & 4) and 255 or 85)
        return "#%02x%02x%02x" % (r, g, b)
    if n < 232:
        n -= 16
        lv = [0, 95, 135, 175, 215, 255]
        return "#%02x%02x%02x" % (lv[n // 36], lv[(n // 6) % 6], lv[n % 6])
    v = 8 + (n - 232) * 10
    return "#%02x%02x%02x" % (v, v, v)


class State:
    __slots__ = ("fg", "bg", "bold", "dim", "rev")

    def __init__(self):
        self.reset()

    def reset(self):
        self.fg = self.bg = None
        self.bold = self.dim = self.rev = False

    def key(self):
        fg = self.fg or DEFAULT_FG
        bg = self.bg or DEFAULT_BG
        if self.rev:
            fg, bg = bg, fg
        return fg, bg, self.bold, self.dim


def apply_sgr(st, params):
    codes = [int(p) if p else 0 for p in params.split(";")] or [0]
    i = 0
    while i < len(codes):
        c = codes[i]
        if c == 0:
            st.reset()
        elif c == 1:
            st.bold = True
        elif c == 2:
            st.dim = True
        elif c == 7:
            st.rev = True
        elif c in (22, 27):
            st.bold = st.dim = st.rev = False
        elif c == 39:
            st.fg = None
        elif c == 49:
            st.bg = None
        elif 30 <= c <= 37:
            st.fg = NAMED[c - 30]
        elif 40 <= c <= 47:
            st.bg = NAMED[c - 40]
        elif c in (38, 48):
            target = "fg" if c == 38 else "bg"
            if i + 1 < len(codes) and codes[i + 1] == 2:
                r, g, b = (codes[i + 2:i + 5] + [0, 0, 0])[:3]
                setattr(st, target, "#%02x%02x%02x" % (r & 255, g & 255, b & 255))
                i += 4
            elif i + 1 < len(codes) and codes[i + 1] == 5:
                setattr(st, target, xterm256(codes[i + 2] if i + 2 < len(codes) else 0))
                i += 2
        i += 1


def parse(text):
    """-> list of rows, each a list of (char, fg, bg, bold, dim)."""
    rows = []
    st = State()
    for raw in text.replace("\r", "").split("\n"):
        cells = []
        pos = 0
        for m in SGR_RE.finditer(raw):
            for ch in raw[pos:m.start()]:
                cells.append((ch,) + st.key())
            apply_sgr(st, m.group(1))
            pos = m.end()
        for ch in raw[pos:]:
            cells.append((ch,) + st.key())
        rows.append(cells)
        st.reset()
    while rows and not any(c[0].strip() for c in rows[-1]):
        rows.pop()
    return rows


def runs(cells, index, blank_ok):
    """Group consecutive cells sharing the value at `index` into runs."""
    out, start, cur, buf = [], 0, None, []
    for x, cell in enumerate(cells):
        val = cell[index] if index is not None else (cell[1], cell[3], cell[4])
        if val != cur:
            if buf and (blank_ok or cur is not None):
                out.append((start, cur, buf))
            start, cur, buf = x, val, []
        buf.append(cell[0])
    if buf:
        out.append((start, cur, buf))
    return out


def build(rows, title):
    cols = max(len(r) for r in rows)
    term_w = cols * CELL_W
    term_h = len(rows) * CELL_H
    w = term_w + 2 * PAD_X
    h = term_h + TITLEBAR + 2 * PAD_TOP
    p = []
    p.append(
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0f}" height="{h:.0f}" '
        f'viewBox="0 0 {w:.2f} {h:.2f}" font-family="{FONT_STACK}" '
        f'font-size="{FONT_SIZE}" role="img">'
    )
    p.append(f'<rect width="{w:.2f}" height="{h:.2f}" rx="{RADIUS}" fill="{PAGE_BG}"/>')
    p.append(
        f'<path d="M0 {RADIUS}a{RADIUS} {RADIUS} 0 0 1 {RADIUS} -{RADIUS}h{w - 2 * RADIUS:.2f}'
        f'a{RADIUS} {RADIUS} 0 0 1 {RADIUS} {RADIUS}v{TITLEBAR - RADIUS:.2f}H0Z" fill="{CHROME_BG}"/>'
    )
    p.append(f'<rect x="0" y="{TITLEBAR:.2f}" width="{w:.2f}" height="1" fill="{CHROME_LINE}"/>')
    for i in range(3):
        p.append(f'<circle cx="{16 + i * 15}" cy="{TITLEBAR / 2:.1f}" r="4.5" fill="{CHROME_LINE}"/>')
    p.append(
        f'<text x="{w / 2:.2f}" y="{TITLEBAR / 2 + 4:.1f}" text-anchor="middle" '
        f'font-size="11" fill="#6b7488">{escape(title)}</text>'
    )

    ox, oy = PAD_X, TITLEBAR + PAD_TOP
    p.append(
        f'<rect x="{ox - 4:.2f}" y="{oy - 4:.2f}" width="{term_w + 8:.2f}" '
        f'height="{term_h + 8:.2f}" fill="{DEFAULT_BG}"/>'
    )

    # Backgrounds: one rect per run of identical bg (default bg is already painted).
    for y, cells in enumerate(rows):
        for start, bg, chars in runs(cells, 2, True):
            if bg == DEFAULT_BG:
                continue
            p.append(
                f'<rect x="{ox + start * CELL_W:.2f}" y="{oy + y * CELL_H:.2f}" '
                f'width="{len(chars) * CELL_W:.2f}" height="{CELL_H:.2f}" fill="{bg}"/>'
            )

    # Text: one tspan per run of identical (fg, bold, dim); blanks dropped.
    for y, cells in enumerate(rows):
        line = []
        for start, style, chars in runs(cells, None, True):
            # U+2800 is the empty braille cell — it paints nothing, so a run
            # made only of blanks is pure weight in the file.
            text = "".join(chars)
            if not text.strip(" ⠀"):
                continue
            fg, bold, dim = style
            # Pin the run to its cells: glyphs from fallback fonts (✳ ▼ “)
            # have their own advance widths and would push the rest of the
            # run, and the box borders after it, off the grid.
            attrs = (
                f'x="{ox + start * CELL_W:.2f}" textLength="{len(chars) * CELL_W:.2f}" '
                f'lengthAdjust="spacingAndGlyphs"'
            )
            if bold:
                attrs += ' font-weight="700"'
            if dim:
                attrs += ' opacity=".55"'
            if fg != DEFAULT_FG:
                attrs += f' fill="{fg}"'
            line.append(f"<tspan {attrs}>{escape(text)}</tspan>")
        if line:
            p.append(
                f'<text y="{oy + y * CELL_H + BASELINE:.2f}" fill="{DEFAULT_FG}" '
                f'xml:space="preserve">{"".join(line)}</text>'
            )
    p.append("</svg>")
    return "\n".join(p)


if __name__ == "__main__":
    src, dst = sys.argv[1], sys.argv[2]
    title = sys.argv[3] if len(sys.argv) > 3 else "topterm"
    svg = build(parse(open(src, encoding="utf-8").read()), title)
    open(dst, "w", encoding="utf-8").write(svg)
    print(f"{dst}: {len(svg)} bytes")
