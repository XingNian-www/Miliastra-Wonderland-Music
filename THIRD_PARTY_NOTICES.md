# Third-Party Notices

## FFmpeg

Native audio playback dynamically links a minimized FFmpeg 8.1 Windows x64
build under LGPL-2.1-or-later. The corresponding source URL, configure flags,
runtime hashes, and redistribution notes are recorded in
`vendor/ffmpeg/8.1/windows-x64/README.md`.

- Source: <https://ffmpeg.org/releases/ffmpeg-8.1.tar.xz>
- License: <https://ffmpeg.org/legal.html>

## MNN

The OCR runtime package includes MNN 3.6.0 from Alibaba under the Apache
License 2.0.

- Source: <https://github.com/alibaba/MNN/tree/3.6.0>
- License: <https://github.com/alibaba/MNN/blob/3.6.0/LICENSE.txt>

## Microsoft WebView2 static loader

The login helper statically links `WebView2LoaderStatic.lib` from the
`Microsoft.Web.WebView2` NuGet package version `1.0.4129.50` (Windows x64).
The build-only library is stored at
`vendor/webview2/1.0.4129.50/x64/WebView2LoaderStatic.lib` and has SHA-256
`482f24196b20e784c4d29b752ea760946cb54e22c2532a29699ef538d2d5c28`.

```text
Copyright (C) Microsoft Corporation. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice,
   this list of conditions and the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.
3. Neither the name of Microsoft Corporation nor the names of its contributors
   may be used to endorse or promote products derived from this software
   without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
POSSIBILITY OF SUCH DAMAGE.
```

## chinese-xinhua idiom lexicon

`assets/idioms.txt` is derived from the `word`, `derivation`, and `explanation`
fields of `data/idiom.json` in
[`pwxcoo/chinese-xinhua`](https://github.com/pwxcoo/chinese-xinhua), pinned at
commit [`fe6d6c2e8baa82187f4c96bbe042e43f96c05666`](https://github.com/pwxcoo/chinese-xinhua/tree/fe6d6c2e8baa82187f4c96bbe042e43f96c05666).

- Source data: [data/idiom.json](https://github.com/pwxcoo/chinese-xinhua/blob/fe6d6c2e8baa82187f4c96bbe042e43f96c05666/data/idiom.json)
- Source license: [MIT](https://github.com/pwxcoo/chinese-xinhua/blob/fe6d6c2e8baa82187f4c96bbe042e43f96c05666/LICENSE)
- Derived file: 49,674 non-comment records with 49,644 unique idiom keys.
- Derived file SHA-256: `60a3b05a09ed5b909a1e75f5f3d237637d65cf80ff6eadac2ef8bc852ec51cd0`

The source repository's README states that its data was collected and organized from online sources. This project retains the idiom text, derivation, and explanation fields with this attribution; it does not independently verify the provenance of every source record.

```text
MIT License

Copyright (c) 2018 PWXCOO

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## pinyin

Homophone idiom matching uses [`pinyin`](https://github.com/mozillazg/rust-pinyin)
version 0.11.0 under the MIT License.

```text
The MIT License (MIT)

Copyright (c) 2016 mozillazg

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
