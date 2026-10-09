#ifndef WEBRTC_MF_H264_ENCODER_IMPL_H_
#define WEBRTC_MF_H264_ENCODER_IMPL_H_

#include <memory>
#include <string>
#include <vector>

#include "api/environment/environment.h"
#include "api/video_codecs/h264_profile_level_id.h"
#include "api/video_codecs/sdp_video_format.h"
#include "api/video_codecs/video_encoder.h"
#include "modules/video_coding/include/video_codec_interface.h"

namespace webrtc {

namespace mf {
class EncoderSession;
}

// H.264 on a hardware Media Foundation transform (NVENC, AMF or Quick Sync
// behind Windows' own interface), fed system-memory NV12. Meant to sit inside
// a VideoEncoderSoftwareFallbackWrapper: every failure is reported so that
// OpenH264 takes over rather than the stream stopping.
class MediaFoundationH264EncoderImpl : public VideoEncoder {
 public:
  MediaFoundationH264EncoderImpl(const Environment& env,
                                 const SdpVideoFormat& format);
  ~MediaFoundationH264EncoderImpl() override;

  int32_t InitEncode(const VideoCodec* codec_settings,
                     const Settings& settings) override;

  int32_t RegisterEncodeCompleteCallback(
      EncodedImageCallback* callback) override;

  int32_t Release() override;

  int32_t Encode(const VideoFrame& frame,
                 const std::vector<VideoFrameType>* frame_types) override;

  void SetRates(const RateControlParameters& parameters) override;

  EncoderInfo GetEncoderInfo() const override;

 private:
  int32_t OpenSession(int width, int height);
  void CloseSession();

  const Environment& env_;
  H264Profile profile_ = H264Profile::kProfileConstrainedBaseline;
  EncodedImageCallback* callback_ = nullptr;
  VideoCodec codec_ = {};
  std::shared_ptr<mf::EncoderSession> session_;
  std::string implementation_name_ = "MediaFoundation";
  uint32_t target_bps_ = 0;
  double framerate_ = 0;
  bool sending_ = false;
  bool key_frame_pending_ = true;
  // A transform that ignores key frame requests leaves new viewers without a
  // picture; one that has not produced one this many frames after being asked
  // is given up on.
  bool awaiting_key_frame_ = false;
  int frames_since_key_request_ = 0;
  int64_t last_sample_time_ = -1;
  // When frames started being dropped for want of an input slot; a transform
  // that stays stuck that long is given up on.
  int64_t stalled_since_ms_ = -1;
};

}  // namespace webrtc

#endif  // WEBRTC_MF_H264_ENCODER_IMPL_H_
