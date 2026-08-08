# FFmpeg 8.1 Windows x64 SDK

This directory contains the headers, import libraries, and runtime DLLs used by
the sole native playback backend. The DLLs were built from FFmpeg 8.1 for
MinGW-w64 and are dynamically linked so the application remains under its own
license while FFmpeg remains replaceable under LGPL-2.1-or-later.

Source: <https://ffmpeg.org/releases/ffmpeg-8.1.tar.xz>

Configure command recorded by `avutil_configuration()`:

```text
--prefix=/tmp/miliastra-audio-ffmpeg --arch=x86_64 --target-os=mingw32
--enable-shared --disable-static --enable-small --disable-autodetect
--disable-programs --disable-doc --disable-debug --disable-everything
--disable-avdevice --disable-indevs --disable-outdevs --disable-hwaccels
--disable-filters --disable-pthreads --enable-w32threads --enable-avcodec
--enable-avformat --enable-avutil --enable-avfilter --enable-swresample
--enable-swscale --enable-protocol=file,http,https,tcp,tls --enable-schannel
--enable-demuxer=aac,flac,matroska,mov,mp3,ogg,wav
--enable-decoder=aac,aac_fixed,alac,flac,mp3,mp3float,opus,vorbis,pcm_alaw,
pcm_f32le,pcm_f64le,pcm_mulaw,pcm_s16be,pcm_s16le,pcm_s24le,pcm_s32le,pcm_u8
--enable-parser=aac,flac,mpegaudio,opus,vorbis
--enable-bsf=aac_adtstoasc,extract_extradata
--extra-cflags=-ffunction-sections -fdata-sections
--extra-ldflags=-Wl,--gc-sections -static-libgcc
```

Only `avformat`, `avcodec`, `avutil`, and `swresample` are linked by the Rust
crate. The package script rejects `avfilter`, `avdevice`, `swscale`, and any
other media DLL even though the upstream build produced unused libraries.

Runtime SHA-256:

```text
avformat-62.dll     237041e77e28137aa3567dc6063594184d4bda7f0c9141e9157577c310459fdb
avcodec-62.dll      8dbc1c3de1fe9cc5242a37c76ab4e209c54f8adba2354224deed62413244356d
avutil-60.dll       48ae383a29f701611a09097d3c4bf564d7588ec7dd655a624b3d66787a754fb1
swresample-6.dll    a9d5f887db59bddac1c9671c77b58a637d4c2fdc288b4fd281f2241758e3104c
libwinpthread-1.dll 5c978176fd66590dad8cbf76c161a01872d6bf2f956c47a04bdde983a7ec0627
```

FFmpeg license and source-distribution requirements are documented at
<https://ffmpeg.org/legal.html>. Distributors must preserve the LGPL notices
and provide the corresponding FFmpeg source and build configuration.
