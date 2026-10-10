# Vendored webrtc-sys

This is `webrtc-sys` **0.3.48** from crates.io (LiveKit's bindings, pulled in by `libwebrtc =
"0.3.51"`), copied from the registry source minus `.cargo-ok` and `.cargo_vcs_info.json`, and
used instead of the published crate through `client/Cargo.toml`:

```toml
[patch.crates-io]
webrtc-sys = { path = "vendor/webrtc-sys" }
```

Why: upstream's `build.rs` compiles no hardware video codec on Windows (NVENC and VAAPI exist only
for Linux), so H.264 runs on OpenH264 on the CPU. A 2560×1440 screen sent as 1080p60 took about a
whole core. This copy adds hardware H.264 encoding through Media Foundation, which reaches NVENC,
AMD's VCN and Intel's Quick Sync through the drivers' own encoder transforms, with nothing to
install beyond the Windows SDK.

## What differs from upstream 0.3.48

`git diff --no-index <registry>/webrtc-sys-0.3.48 client/vendor/webrtc-sys` shows all of it.

1. **`build.rs`**, `"windows"` arm: links `mfplat` and `mfuuid`, compiles
   `src/mf/mf_common.cpp`, `src/mf/h264_encoder_impl.cpp` and `src/mf/mf_encoder_factory.cpp`,
   and defines `USE_MEDIA_FOUNDATION_VIDEO_CODEC`.
2. **`src/video_encoder_factory.cpp`**, three hunks, all behind `USE_MEDIA_FOUNDATION_VIDEO_CODEC`:
   - includes `mf/mf_encoder_factory.h`;
   - `AddMediaFoundationFactory()`, called from `InternalFactory::InternalFactory()` right after
     `AddJetsonFactory()`, registers the factory as the `Hardware` backend when a hardware H.264
     encoder transform exists;
   - `video_encoder_backend_list()` then reports `Hardware`.
3. **`src/mf/`** (new):
   - `mf_common.*`: COM and Media Foundation start-up, enumeration of hardware transforms adapter
     by adapter (discrete cards first, `MFTEnum2`), and a D3D11 device plus `IMFDXGIDeviceManager`
     on a given adapter.
   - `mf_encoder_factory.*`: advertises H.264 Constrained High, High, Main, Constrained Baseline
     and Baseline, packetization mode 1, and wraps every encoder it creates in
     `CreateVideoEncoderSoftwareFallbackWrapper` with OpenH264 behind it.
   - `h264_encoder_impl.*`: the encoder. Asynchronous transform driven by its event generator
     (no polling, no queueing beyond the transform's own); system-memory NV12 input (an NV12
     frame is lent without copying, anything else converted with libyuv); a D3D11 device only for
     transforms that refuse media types without one (AMD's); CBR, low-latency mode, no B-frames,
     ten-minute GOP with IDR on request, bitrate updated in `SetRates`; Annex B output with
     SPS/PPS prepended from `MF_MT_MPEG_SEQUENCE_HEADER` if a transform leaves them out. Any
     failure (no transform opens, `ProcessInput`/`ProcessOutput` errors, no input slot for 2 s,
     a key frame request ignored for 60 frames, reordered output) returns
     `WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE`, and the wrapper continues on OpenH264.
   - `h264_decoder_impl.*`: a DXVA H.264 decoder. **Not compiled** (see below).

4. **`src/adm_proxy.cpp`**, `AdmProxy::Terminate()`: no longer terminates the sub-ADMs; they live
   as long as the proxy and are terminated in `~AdmProxy`, as upstream issue
   [livekit/rust-sdks#1468](https://github.com/livekit/rust-sdks/issues/1468) proposes. WebRTC
   terminates the ADM when the last peer connection closes and calls the no-op `Init()` for the
   next one; the synthetic ADM's 10 ms pumping task died with `Terminate`, so upstream never
   pulled remote audio again: after leaving a channel, a call ending or being left alone in one,
   every voice received played nothing until the app restarted.
   `voice_plays_after_a_private_call` in `src/media/rtc.rs` covers it.

Nothing else is changed; the upstream files keep their bytes.

## Environment variables

- `LIVEKIT_MF_H264_ENCODER`: `off` hides the hardware encoder (then `list_available()` has no
  `Hardware`); `nvidia`, `amd` or `intel` picks a vendor; anything else is matched against the
  transform's name. Unset, the first transform that opens wins, discrete cards first.
- `LIVEKIT_MF_H264_DECODER=off`: only read by the uncompiled decoder.

## The decoder that is left out

`src/mf/h264_decoder_impl.*` decodes through Windows' H.264 decoder transform on a D3D11 device
and reads each NV12 surface back into an I420 buffer. Measured on the viewer of a 1080p60 share
(RTX 5070), it cost as much CPU as FFmpeg (about 49% of a core against 47%, with or without a
fence instead of a blocking `Map`): the readback eats what the decode saves. It becomes worth it
once decoded frames can stay on the GPU up to the screen. To wire it:

- `build.rs`: `.file("src/mf/h264_decoder_impl.cpp")`, `.define("USE_MEDIA_FOUNDATION_VIDEO_DECODER", "1")`
  and `println!("cargo:rustc-link-lib=dylib=dxguid")`;
- `src/video_decoder_factory.cpp`, where it returns `webrtc::H264Decoder::Create()`: when
  `webrtc::MediaFoundationH264DecoderImpl::IsSupported()`, return
  `webrtc::CreateVideoDecoderSoftwareFallbackWrapper(env, webrtc::H264Decoder::Create(), std::make_unique<webrtc::MediaFoundationH264DecoderImpl>())`
  instead (includes: `mf/h264_decoder_impl.h`,
  `api/video_codecs/video_decoder_software_fallback_wrapper.h`).

## Upgrading webrtc-sys

1. Copy the new version from the registry over this directory, keeping `src/mf/` and this file.
2. Re-apply the `build.rs` block, the three `video_encoder_factory.cpp` hunks and the
   `AdmProxy::Terminate` fix above, unless #1468 is fixed by then (run
   `voice_plays_after_a_private_call` without it to see).
3. Build and check what moved in the APIs used by `src/mf`: `VideoEncoder`/`EncoderInfo`,
   `CreateVideoEncoderSoftwareFallbackWrapper`, `H264::FindNaluIndices` and
   `H264BitstreamParser::ParseBitstream` (both take `std::span`), `NV12BufferInterface`.
4. `cargo test` in `client` runs `hardware_encoder_streams`, a loopback call that fails unless
   the Media Foundation encoder carries the stream.

To drop the vendoring instead, remove the `[patch.crates-io]` lines and this directory; Cargo.lock
gets back the registry `source` and `checksum` lines for webrtc-sys.
