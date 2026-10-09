#include "mf_encoder_factory.h"

#include "api/video_codecs/h264_profile_level_id.h"
#include "api/video_codecs/video_encoder_software_fallback_wrapper.h"
#include "h264_encoder_impl.h"
#include "mf_common.h"
#include "modules/video_coding/codecs/h264/include/h264.h"
#include "rtc_base/logging.h"

namespace webrtc {

MediaFoundationVideoEncoderFactory::MediaFoundationVideoEncoderFactory() {
  // Packetization mode 1 only: these encoders cannot bound their NAL units
  // to a packet. Level 5.2 so the level never caps what is sent. Baseline is
  // served with Constrained Baseline, a subset of it; it is what MediaMTX
  // negotiates.
  for (H264Profile profile :
       {H264Profile::kProfileConstrainedHigh, H264Profile::kProfileHigh,
        H264Profile::kProfileMain, H264Profile::kProfileConstrainedBaseline,
        H264Profile::kProfileBaseline}) {
    supported_formats_.push_back(
        CreateH264Format(profile, H264Level::kLevel5_2, "1"));
  }
}

MediaFoundationVideoEncoderFactory::~MediaFoundationVideoEncoderFactory() =
    default;

bool MediaFoundationVideoEncoderFactory::IsSupported() {
  static const bool supported = [] {
    std::vector<mf::MftCandidate> candidates = mf::EnumerateHardwareMfts(
        MFT_CATEGORY_VIDEO_ENCODER, MFVideoFormat_NV12, MFVideoFormat_H264,
        mf::kEncoderOverrideEnv);
    for (const mf::MftCandidate& candidate : candidates) {
      RTC_LOG(LS_INFO) << "Hardware H.264 encoder transform: " << candidate.name
                       << " (" << candidate.vendor_id << ", adapter "
                       << (candidate.adapter_luid
                               ? std::to_string(candidate.adapter_luid->LowPart)
                               : std::string("unknown"))
                       << ")";
    }
    if (candidates.empty()) {
      RTC_LOG(LS_INFO) << "No hardware H.264 encoder transform; H.264 is "
                          "encoded in software";
    }
    return !candidates.empty();
  }();
  return supported;
}

std::unique_ptr<VideoEncoder> MediaFoundationVideoEncoderFactory::Create(
    const Environment& env,
    const SdpVideoFormat& format) {
  if (!format.IsCodecInList(supported_formats_)) {
    return nullptr;
  }
  auto hardware = std::make_unique<MediaFoundationH264EncoderImpl>(env, format);
  if (!H264Encoder::IsSupported()) {
    return hardware;
  }
  // OpenH264 takes over whenever the transform will not open at this size,
  // or fails, stalls or ignores a key frame request mid-stream.
  return CreateVideoEncoderSoftwareFallbackWrapper(
      env, CreateH264Encoder(env, H264EncoderSettings::Parse(format)),
      std::move(hardware), /*prefer_temporal_support=*/false);
}

std::vector<SdpVideoFormat>
MediaFoundationVideoEncoderFactory::GetSupportedFormats() const {
  return supported_formats_;
}

std::vector<SdpVideoFormat>
MediaFoundationVideoEncoderFactory::GetImplementations() const {
  return supported_formats_;
}

}  // namespace webrtc
