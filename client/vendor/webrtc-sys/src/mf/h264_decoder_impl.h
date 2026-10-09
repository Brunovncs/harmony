#ifndef WEBRTC_MF_H264_DECODER_IMPL_H_
#define WEBRTC_MF_H264_DECODER_IMPL_H_

#include <d3d11_4.h>

#include <cstdint>
#include <deque>
#include <optional>
#include <string>

#include "api/video/color_space.h"
#include "api/video_codecs/video_decoder.h"
#include "common_video/h264/h264_bitstream_parser.h"
#include "common_video/include/video_frame_buffer_pool.h"
#include "mf_common.h"

namespace webrtc {

// H.264 decoded by the graphics card through DXVA: Windows' own H.264 decoder
// transform given a D3D11 device, its NV12 surfaces read back into I420.
// Meant to sit inside a VideoDecoderSoftwareFallbackWrapper: anything it
// cannot do is reported so that FFmpeg takes over.
//
// Not compiled: reading every surface back to system memory cost as much CPU
// as FFmpeg decoding the same 1080p60 share. Worth wiring once frames can stay
// on the GPU up to the screen; VENDORED.md has the three lines it takes.
class MediaFoundationH264DecoderImpl : public VideoDecoder {
 public:
  MediaFoundationH264DecoderImpl();
  ~MediaFoundationH264DecoderImpl() override;

  // Whether this machine decodes H.264 in hardware through D3D11. Cached.
  static bool IsSupported();

  bool Configure(const Settings& settings) override;
  int32_t Decode(const EncodedImage& input_image,
                 bool missing_frames,
                 int64_t render_time_ms) override;
  int32_t RegisterDecodeCompleteCallback(
      DecodedImageCallback* callback) override;
  int32_t Release() override;
  DecoderInfo GetDecoderInfo() const override;

 private:
  struct Pending {
    int64_t sample_time = 0;
    uint32_t rtp_timestamp = 0;
    int64_t submitted_us = 0;
    std::optional<ColorSpace> color_space;
  };

  bool Open();
  bool SetOutputType();
  // Collects every frame the transform has ready. False on an error that
  // the software decoder should take over from.
  bool Drain();
  bool Deliver(IMFSample* sample);
  bool ReadBack(ID3D11Texture2D* texture,
                UINT subresource,
                uint8_t* y,
                int stride_y,
                uint8_t* u,
                int stride_u,
                uint8_t* v,
                int stride_v);

  std::optional<mf::D3DDevice> d3d_;
  mf::ComPtr<ID3D11DeviceContext> context_;
  mf::ComPtr<ID3D11Texture2D> staging_;
  mf::ComPtr<ID3D11DeviceContext4> context4_;
  mf::ComPtr<ID3D11Fence> fence_;
  HANDLE fence_event_ = nullptr;
  UINT64 fence_value_ = 0;
  mf::ComPtr<IMFTransform> transform_;
  DWORD input_id_ = 0;
  DWORD output_id_ = 0;
  int width_ = 0;
  int height_ = 0;
  bool saw_key_frame_ = false;
  bool reported_ = false;
  std::deque<Pending> pending_;
  DecodedImageCallback* callback_ = nullptr;
  VideoFrameBufferPool buffer_pool_;
  H264BitstreamParser parser_;
};

}  // namespace webrtc

#endif  // WEBRTC_MF_H264_DECODER_IMPL_H_
