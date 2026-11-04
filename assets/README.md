# Bundled fonts

Vendored deliberately rather than fetched at build time: the runtime image is
`scratch` and has no system fonts, so these files are the *only* way a glyph
reaches the card. Committing them keeps `docker build` network-free and makes
the rendered output reproducible.

All four are SIL Open Font License 1.1.

| File | Family | Role | Source |
|---|---|---|---|
| `Figtree.ttf` | Figtree | Latin — brand display face (variable, covers Regular→Bold) | [google/fonts `ofl/figtree`](https://github.com/google/fonts/tree/main/ofl/figtree) |
| `NotoColorEmoji.ttf` | Noto Color Emoji | Emoji (CBDT bitmap) | [googlefonts/noto-emoji](https://github.com/googlefonts/noto-emoji) |
| `NotoSans.ttf` | Noto Sans | Latin/Greek/Cyrillic beyond Figtree (variable) | [google/fonts `ofl/notosans`](https://github.com/google/fonts/tree/main/ofl/notosans) |
| `NotoSansJP.ttf` | Noto Sans JP | CJK — kana + kanji (variable) | [google/fonts `ofl/notosansjp`](https://github.com/google/fonts/tree/main/ofl/notosansjp) |

## Wordmark

`wordmark-white.svg` is copied from `Brainstorm-UI/client/public/brand/`. It is
`include_str!`d into the binary rather than read from this directory at runtime,
so the card cannot lose its branding because of a bad `ASSETS_DIR`. Its own fill
is `#F2F3F0` — slate-50, the same token the card uses for primary text — so it
needs no recolouring on the dark surface. Re-copy it if the brand asset changes.

SHA-256:

```
26ad3db9b31ff7dde67a91ff515d022d2f495cd506590699cf264f0bfe6fb714  Figtree.ttf
72a635cb3d2f3524c51620cdde406b217204e8a6a06c6a096ff8ed4b5fd6e27b  NotoColorEmoji.ttf
bfb7bb691513f12e734dc346c03a03f784912432d7e3fa8e56efcf906fe86b3d  NotoSans.ttf
c2f3b4d463500a2ddcd3849cded1fceeb9fd6d1c32e6cbecd568453ba50fc68f  NotoSansJP.ttf
```

## Why these four

Nostr display names are heavily emoji and CJK. DejaVu (what this image used to
install from Alpine) covers neither, so those names rendered as tofu boxes on
the single most-shared surface the product has. Figtree has no Cyrillic, and
Noto Sans JP only the Russian alphabet, so Ukrainian, Serbian and Kazakh
letters (і, ђ, қ …) need Noto Sans.

Faces load in file-name order, which is resvg's fallback order: the first
fallback with the first missing character also takes every other one it has.

Verified against resvg 0.47 before committing to the ~19 MB: colour emoji
reaches the canvas as a real bitmap glyph. usvg detects the CBDT table and emits
the glyph as an image node (`usvg/src/text/flatten.rs`), which resvg decodes via
`tiny_skia::Pixmap::decode_png` (`resvg/src/image.rs:68-81`). Both the `text`
and `raster-images` features are on by default. Glyph-level fallback across all
the families is automatic — one string mixing Latin, emoji and kanji resolves
each run to the right face with no explicit fallback configuration.

## Known gap

Hangul is not covered — Noto Sans JP carries kana and kanji only. Korean names
hit the unrenderable-codepoint backstop in `render.rs` and degrade to a clean
trim rather than boxes. Add Noto Sans KR (~5 MB) if that becomes common.
Flag emoji (regional-indicator pairs) are stripped before drawing: resvg can't
shape them, and a miss stops fallback for the rest of the line.

## Changing a font

`font_family` in `config.rs` must keep matching a family name here exactly; it
is no longer inferred from whichever face happens to load first. The service
refuses to start when this directory yields zero faces, so a bad swap fails at
boot rather than silently shipping textless cards.
