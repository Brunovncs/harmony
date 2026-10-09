#ifndef WEBRTC_MF_ENCODER_FACTORY_H_
#define WEBRTC_MF_ENCODER_FACTORY_H_

#include <memory>
#include <vector>

#include "api/environment/environment.h"
#include "api/video_codecs/sdp_video_format.h"
#include "api/video_codecs/video_encoder_factory.h"

namespace webrtc {

// H.264 on the GPU through Media Foundation, for Windows. Each encoder it
// creates falls back to OpenH264 by itself when the hardware one cannot open
// or fails mid-stream.
class MediaFoundationVideoEncoderFactory : public VideoEncoderFactory {
 public:
  MediaFoundationVideoEncoderFactory();
  ~MediaFoundationVideoEncoderFactory() override;

  // Whether a hardware H.264 encoder transform is installed. Cached.
  static bool IsSupported();

  std::unique_ptr<VideoEncoder> Create(const Environment& env,
                                       const SdpVideoFormat& format) override;

  std::vector<SdpVideoFormat> GetSupportedFormats() const override;

  std::vector<SdpVideoFormat> GetImplementations() const override;

 private:
  std::vector<SdpVideoFormat> supported_formats_;
};

}  // namespace webrtc

#endif  // WEBRTC_MF_ENCODER_FACTORY_H_
