# Bundled font

`JetBrainsMono-Regular.ttf` is **JetBrains Mono** (Regular weight), the monospace
font bundled and embedded into the status-bar renderer so text renders identically
on any host — including a font-less headless CI box.

JetBrains Mono is released under the **SIL Open Font License, Version 1.1**
(OFL-1.1) — a permissive font license that explicitly allows bundling, embedding,
modification and redistribution. It is **not** GPL. The full license text ships
alongside the font as `JetBrainsMono-OFL.txt` (as the OFL requires).

Source: https://github.com/JetBrains/JetBrainsMono (release v2.304)

JetBrains Mono covers every glyph the UI uses: Latin + German umlauts (ä ö ü ß),
the transfer arrows ↓ ↑ →, the warning sign ⚠ and the ellipsis … — so no fallback
face is needed and the renderer loads exactly this one font.
