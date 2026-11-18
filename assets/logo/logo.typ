// The ww_tape logo: the mark (a play triangle built from three tracks of blocks, the newest block at its tip in red)
// and the wordmark. One grid drives the mark: u is a pixel of the 32 px icon, every lane is 8u tall and every gap
// is u wide, so the triangle is 26u tall and each cut falls on a whole u.
// render.typ picks a variant and a size; `just logo` exports them all.

// WireWeaver's red and ink (wire_weaver's docs/assets/src/logos.py).
#let themes = (
  light: (ink: rgb("#1a1a1a"), red: rgb("#c8141c")),
  dark: (ink: rgb("#ececec"), red: rgb("#d81e1e")),
)
// The icon's tile and the social card are dark, whatever the page around them.
#let tile-fill = rgb("#0f0f10")

#let u = 16pt
#let gap = u
#let lane = 8 * u
#let lanes = 3
#let mark-height = lanes * lane + (lanes - 1) * gap
#let mark-width = mark-height * calc.sqrt(3) / 2
// Block cuts per lane, in u from the triangle's left edge. The middle lane's last block is the newest: "tip" cuts
// where the triangle's edges meet that lane, so the red block is a small triangle of its own, the tip.
#let cuts = ((7,), (6, "tip"), (5,))
// For sizes up to 48 px: only the cut of the newest block, and lanes three times the gap, so that at 11/16 of a
// 16, 32 or 48 px tile every lane and gap is whole pixels (3 + 1 + 3 + 1 + 3 at 16 px).
#let cuts-small = ((), ("tip",), ())
#let gap-small = mark-height / 11

// The triangle's corners: top left, tip, bottom left.
#let tri = ((0pt, 0pt), (mark-width, mark-height / 2), (0pt, mark-height))

// A convex polygon clipped to one side of an axis-parallel line: keep x >= at (axis "x", keep "above"), and so on.
#let clip(pts, axis, at, keep) = {
  let i = if axis == "x" { 0 } else { 1 }
  let inside(p) = if keep == "above" { p.at(i) >= at } else { p.at(i) <= at }
  let out = ()
  for k in range(pts.len()) {
    let p = pts.at(k)
    let q = pts.at(calc.rem(k + 1, pts.len()))
    if inside(p) { out.push(p) }
    if inside(p) != inside(q) {
      let t = (at - p.at(i)) / (q.at(i) - p.at(i))
      out.push((p.at(0) + t * (q.at(0) - p.at(0)), p.at(1) + t * (q.at(1) - p.at(1))))
    }
  }
  out
}

// The mark alone, mark-width × mark-height, transparent around it.
#let mark(theme: "light", small: false) = block(width: mark-width, height: mark-height, {
  let t = themes.at(theme)
  let g = if small { gap-small } else { gap }
  let lane = (mark-height - (lanes - 1) * g) / lanes
  for (j, lane-cuts) in (if small { cuts-small } else { cuts }).enumerate() {
    let y = j * (lane + g)
    let strip = clip(clip(tri, "y", y, "above"), "y", y + lane, "below")
    let tip = mark-width * (y / (mark-height / 2)) - g / 2
    let edges = (0pt,) + lane-cuts.map(c => if c == "tip" { tip } else { c * u }) + (mark-width,)
    for k in range(edges.len() - 1) {
      let last = k == edges.len() - 2
      let l = edges.at(k) + if k == 0 { 0pt } else { g / 2 }
      let r = edges.at(k + 1) - if last { 0pt } else { g / 2 }
      let piece = clip(clip(strip, "x", l, "above"), "x", r, "below")
      let red = j == calc.quo(lanes, 2) and last
      place(top + left, polygon(..piece, fill: if red { t.red } else { t.ink }, stroke: none))
    }
  }
})

// The wordmark: JetBrains Mono Bold as WireWeaver's, the underscore a red bar as thick as the letters' stems.
#let word-size = 220pt
#let word(s, fill) = text(font: "JetBrains Mono", weight: "bold", size: word-size, fill: fill,
  top-edge: "ascender", bottom-edge: "descender", s)
#let wordmark-size = (width: 7 * 0.6 * word-size, height: (1.02 + 0.3) * word-size)
#let wordmark(theme: "light") = block(..wordmark-size, {
  let t = themes.at(theme)
  let adv = 0.6 * word-size
  place(top + left, word("ww", t.ink))
  place(top + left, dx: 3 * adv, word("tape", t.ink))
  // The bar rests on the baseline, 0.3em above the bottom (the descender).
  let bar = 0.15 * word-size
  place(top + left, dx: 2 * adv + 0.08 * adv, dy: wordmark-size.height - 0.3 * word-size - bar,
    rect(width: 0.84 * adv, height: bar, fill: t.red, stroke: none))
})

// The mark above the wordmark, with no gap but the wordmark's own room above its letters (its top is the font's
// ascender). A triangle's visual centre lies right of its box's centre: the mark moves right by a twelfth of its width.
#let lockup-gap = 0pt
#let optical = mark-width / 12
#let lockup-size = (width: wordmark-size.width, height: mark-height + lockup-gap + wordmark-size.height)
#let lockup(theme: "light") = block(..lockup-size, {
  place(top + center, dx: optical, mark(theme: theme))
  place(bottom + left, wordmark(theme: theme))
})

// The app icon: the dark mark on a dark rounded tile, for avatars and rustdoc, on any background. The small one is
// for favicons and sizes up to 48 px: the small mark, 11/16 of the tile tall, its top 2/16 and its left 4/16 of the
// tile in, so its lanes fall on whole pixels.
#let icon-size = 680pt
#let icon(small: false) = block(width: icon-size, height: icon-size, radius: 22%, fill: tile-fill, clip: true, {
  if small {
    let s = icon-size * 11 / 16 / mark-height
    place(top + left, dx: icon-size * 4 / 16, dy: icon-size * 2 / 16,
      scale(s * 100%, origin: top + left, reflow: true, mark(theme: "dark", small: true)))
  } else {
    let s = icon-size * 0.6 / mark-height
    place(center + horizon, dx: optical * s, scale(s * 100%, reflow: true, mark(theme: "dark")))
  }
})
