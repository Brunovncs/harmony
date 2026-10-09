# Third-party material in the client

Everything here ships inside `harmony.exe`.

| What | Where | Licence |
| --- | --- | --- |
| Figtree, by Erik Kennedy and the Figtree Project Authors (static Regular to Bold instances of the variable font, cut down to Latin, Greek and Cyrillic) | `assets/fonts/Figtree-*.ttf` | SIL Open Font License 1.1, in `assets/fonts/OFL.txt` |
| Geist Mono, by Vercel | `assets/fonts/GeistMono-*.ttf` | SIL Open Font License 1.1, in `assets/fonts/OFL.txt` |
| MediaPipe selfie segmentation, by Google, as converted to ONNX by onnx-community | `assets/models/selfie_segmentation.onnx` | Apache 2.0 |
| Lucide icons, by the Lucide contributors (path data, every icon but `soundboard` and `blur`) | `src/icons.rs` | ISC, below |
| The text field, the theme's Windows accent reader and the widget patterns, from OpenController by Brunovncs | `src/text_field.rs`, `src/theme.rs`, `src/widgets.rs` | MIT |
| libwebrtc, by the WebRTC project, through LiveKit's `libwebrtc` and `webrtc-sys` crates | linked in; `webrtc-sys` vendored in `vendor/webrtc-sys` with a Media Foundation encoder added | BSD 3-Clause (libwebrtc), Apache 2.0 (bindings) |
| GPUI, by Zed Industries, as published by the gpui-kit maintainers (`gpui-pre`, `gpui-pre-windows`) | vendored in `vendor/` with video surfaces added on Windows (see `vendor/GPUI_VENDORED.md`) | Apache 2.0 |

Harmony's own icon (`assets/icon.ico` and `assets/icon.png`) is drawn in code by `examples/icon.rs`,
and the camera backgrounds in `assets/backgrounds/` by `examples/backgrounds.rs`. They, and the
`soundboard` and `blur` shapes in `src/icons.rs`, are original to this repository and
covered by its MIT licence.

Rust crates are listed with their licences in `Cargo.lock`; `cargo about` or `cargo license`
produces the full list.

## Lucide licence

```
ISC License

Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2022 as part of Feather (MIT).
All other copyright (c) for Lucide are held by Lucide Contributors 2022.

Permission to use, copy, modify, and/or distribute this software for any purpose with or without
fee is hereby granted, provided that the above copyright notice and this permission notice appear
in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH REGARD TO THIS
SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE
AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT,
NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE
OF THIS SOFTWARE.
```

The Feather portions are under the MIT licence:

```
The MIT License (MIT)

Copyright (c) 2013-2022 Cole Bemis

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and
associated documentation files (the "Software"), to deal in the Software without restriction,
including without limitation the rights to use, copy, modify, merge, publish, distribute,
sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or
substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT
NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT
OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```
