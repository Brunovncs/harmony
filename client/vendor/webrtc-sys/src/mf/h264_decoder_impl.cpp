#include "h264_decoder_impl.h"

#include <codecapi.h>
#include <d3d11_4.h>
#include <mferror.h>
#include <strmif.h>
#include <wmcodecdsp.h>

#include <algorithm>
#include <cstdlib>
#include <cstring>
#include <span>
#include <string_view>

#include "api/video/i420_buffer.h"
#include "api/video/video_frame.h"
#include "modules/video_coding/include/video_error_codes.h"
#include "rtc_base/logging.h"
#include "rtc_base/time_utils.h"
#include "third_party/libyuv/include/libyuv/convert.h"

namespace webrtc {

namespace {

constexpr char kOverrideEnv[] = "LIVEKIT_MF_H264_DECODER";

bool DecodesH264(ID3D11Device* device) {
  mf::ComPtr<ID3D11VideoDevice> video;
  if (FAILED(device->QueryInterface(IID_PPV_ARGS(&video)))) {
    return false;
  }
  for (UINT i = 0; i < video->GetVideoDecoderProfileCount(); ++i) {
    GUID profile;
    BOOL supported = FALSE;
    if (SUCCEEDED(video->GetVideoDecoderProfile(i, &profile)) &&
        profile == D3D11_DECODER_PROFILE_H264_VLD_NOFGT &&
        SUCCEEDED(video->CheckVideoDecoderFormat(&profile, DXGI_FORMAT_NV12,
                                                 &supported)) &&
        supported) {
      return true;
    }
  }
  return false;
}

std::optional<LUID> DecodeAdapter() {
  std::vector<LUID> adapters = mf::HardwareAdapters();
  if (adapters.empty()) {
    return std::nullopt;
  }
  return adapters.front();
}

}  // namespace

MediaFoundationH264DecoderImpl::MediaFoundationH264DecoderImpl() = default;

MediaFoundationH264DecoderImpl::~MediaFoundationH264DecoderImpl() {
  Release();
}

bool MediaFoundationH264DecoderImpl::IsSupported() {
  static const bool supported = [] {
    const char* wanted = std::getenv(kOverrideEnv);
    if (wanted && std::string_view(wanted) == "off") {
      return false;
    }
    if (!mf::EnsureComInitialized() || !mf::EnsureMediaFoundationStarted()) {
      return false;
    }
    std::optional<mf::D3DDevice> d3d = mf::CreateD3DDevice(DecodeAdapter());
    const bool decodes = d3d && DecodesH264(d3d->device.Get());
    RTC_LOG(LS_INFO) << "Hardware H.264 decoding through D3D11 is "
                     << (decodes ? "available" : "unavailable");
    return decodes;
  }();
  return supported;
}

bool MediaFoundationH264DecoderImpl::Configure(const Settings& settings) {
  Release();
  if (!mf::EnsureComInitialized() || !Open()) {
    Release();
    return false;
  }
  return true;
}

bool MediaFoundationH264DecoderImpl::Open() {
  d3d_ = mf::CreateD3DDevice(DecodeAdapter());
  if (!d3d_) {
    return false;
  }
  d3d_->device->GetImmediateContext(&context_);
  mf::ComPtr<ID3D11Device5> device5;
  if (SUCCEEDED(d3d_->device.As(&device5)) &&
      SUCCEEDED(context_.As(&context4_)) &&
      SUCCEEDED(device5->CreateFence(0, D3D11_FENCE_FLAG_NONE,
                                     IID_PPV_ARGS(&fence_)))) {
    fence_event_ = CreateEventW(nullptr, FALSE, FALSE, nullptr);
  }

  HRESULT hr =
      CoCreateInstance(CLSID_MSH264DecoderMFT, nullptr, CLSCTX_INPROC_SERVER,
                       IID_PPV_ARGS(&transform_));
  if (FAILED(hr)) {
    RTC_LOG(LS_WARNING) << "H.264 decoder transform unavailable: "
                        << mf::HresultToString(hr);
    return false;
  }
  mf::ComPtr<IMFAttributes> attributes;
  UINT32 d3d11_aware = 0;
  if (FAILED(transform_->GetAttributes(&attributes)) ||
      FAILED(attributes->GetUINT32(MF_SA_D3D11_AWARE, &d3d11_aware)) ||
      !d3d11_aware) {
    return false;
  }
  // Frames out as they come in, without the reordering delay B-frames would
  // need: WebRTC streams have none.
  attributes->SetUINT32(MF_LOW_LATENCY, TRUE);
  mf::ComPtr<ICodecAPI> codec_api;
  if (SUCCEEDED(transform_.As(&codec_api))) {
    VARIANT var = {};
    var.vt = VT_BOOL;
    var.boolVal = VARIANT_TRUE;
    codec_api->SetValue(&CODECAPI_AVLowLatencyMode, &var);
  }

  hr = transform_->ProcessMessage(
      MFT_MESSAGE_SET_D3D_MANAGER,
      reinterpret_cast<ULONG_PTR>(d3d_->manager.Get()));
  if (FAILED(hr)) {
    RTC_LOG(LS_WARNING) << "H.264 decoder refused a D3D11 device: "
                        << mf::HresultToString(hr);
    return false;
  }

  hr = transform_->GetStreamIDs(1, &input_id_, 1, &output_id_);
  if (hr == E_NOTIMPL) {
    input_id_ = 0;
    output_id_ = 0;
  } else if (FAILED(hr)) {
    return false;
  }

  mf::ComPtr<IMFMediaType> type;
  if (FAILED(MFCreateMediaType(&type))) {
    return false;
  }
  type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
  type->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_H264);
  type->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
  hr = transform_->SetInputType(input_id_, type.Get(), 0);
  if (FAILED(hr) || !SetOutputType()) {
    RTC_LOG(LS_WARNING) << "H.264 decoder media types failed: "
                        << mf::HresultToString(hr);
    return false;
  }

  hr = transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
  if (SUCCEEDED(hr)) {
    hr = transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
  }
  return SUCCEEDED(hr);
}

bool MediaFoundationH264DecoderImpl::SetOutputType() {
  for (DWORD i = 0;; ++i) {
    mf::ComPtr<IMFMediaType> type;
    if (FAILED(transform_->GetOutputAvailableType(output_id_, i, &type))) {
      return false;
    }
    GUID subtype;
    if (FAILED(type->GetGUID(MF_MT_SUBTYPE, &subtype)) ||
        subtype != MFVideoFormat_NV12) {
      continue;
    }
    if (FAILED(transform_->SetOutputType(output_id_, type.Get(), 0))) {
      return false;
    }
    UINT32 width = 0;
    UINT32 height = 0;
    MFGetAttributeSize(type.Get(), MF_MT_FRAME_SIZE, &width, &height);
    // Surfaces are padded to whole macroblocks (1088 rows for 1080p); the
    // aperture is the picture.
    MFVideoArea aperture;
    if (SUCCEEDED(type->GetBlob(MF_MT_MINIMUM_DISPLAY_APERTURE,
                                reinterpret_cast<UINT8*>(&aperture),
                                sizeof(aperture), nullptr)) &&
        aperture.Area.cx > 0 && aperture.Area.cy > 0) {
      width = aperture.Area.cx;
      height = aperture.Area.cy;
    }
    width_ = static_cast<int>(width);
    height_ = static_cast<int>(height);
    return true;
  }
}

int32_t MediaFoundationH264DecoderImpl::Decode(const EncodedImage& input_image,
                                               bool missing_frames,
                                               int64_t render_time_ms) {
  if (!transform_ || !callback_) {
    return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
  }
  if (!input_image.data() || input_image.size() == 0) {
    return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
  }
  if (!saw_key_frame_) {
    if (input_image._frameType != VideoFrameType::kVideoFrameKey) {
      return WEBRTC_VIDEO_CODEC_ERROR;
    }
    saw_key_frame_ = true;
  }

  mf::ComPtr<IMFSample> sample;
  mf::ComPtr<IMFMediaBuffer> buffer;
  const DWORD size = static_cast<DWORD>(input_image.size());
  BYTE* data = nullptr;
  if (FAILED(MFCreateSample(&sample)) ||
      FAILED(MFCreateMemoryBuffer(size, &buffer)) ||
      FAILED(buffer->Lock(&data, nullptr, nullptr))) {
    return WEBRTC_VIDEO_CODEC_MEMORY;
  }
  std::memcpy(data, input_image.data(), size);
  buffer->Unlock();
  buffer->SetCurrentLength(size);
  sample->AddBuffer(buffer.Get());

  Pending pending;
  pending.sample_time = pending_.empty() ? 0 : pending_.back().sample_time + 1;
  pending.rtp_timestamp = input_image.RtpTimestamp();
  pending.submitted_us = TimeMicros();
  if (const ColorSpace* color_space = input_image.ColorSpace()) {
    pending.color_space = *color_space;
  }
  sample->SetSampleTime(pending.sample_time);
  if (input_image._frameType == VideoFrameType::kVideoFrameKey) {
    sample->SetUINT32(MFSampleExtension_CleanPoint, TRUE);
  }
  pending_.push_back(pending);
  parser_.ParseBitstream(
      std::span<const uint8_t>(input_image.data(), input_image.size()));

  HRESULT hr = transform_->ProcessInput(input_id_, sample.Get(), 0);
  if (hr == MF_E_NOTACCEPTING) {
    if (!Drain()) {
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    }
    hr = transform_->ProcessInput(input_id_, sample.Get(), 0);
  }
  if (FAILED(hr)) {
    RTC_LOG(LS_WARNING) << "H.264 decoder ProcessInput failed: "
                        << mf::HresultToString(hr)
                        << "; handing over to FFmpeg";
    return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
  }
  return Drain() ? WEBRTC_VIDEO_CODEC_OK : WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
}

bool MediaFoundationH264DecoderImpl::Drain() {
  for (;;) {
    MFT_OUTPUT_STREAM_INFO info = {};
    if (FAILED(transform_->GetOutputStreamInfo(output_id_, &info))) {
      return false;
    }
    MFT_OUTPUT_DATA_BUFFER output = {};
    output.dwStreamID = output_id_;
    mf::ComPtr<IMFSample> sample;
    const bool provides =
        info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES |
                        MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES);
    if (!provides) {
      // The transform only wants samples from the caller when it has fallen
      // back to decoding in software, which FFmpeg does as well.
      RTC_LOG(LS_WARNING) << "H.264 decoder transform is not decoding in "
                             "hardware; handing over to FFmpeg";
      return false;
    }
    DWORD status = 0;
    HRESULT hr = transform_->ProcessOutput(0, 1, &output, &status);
    if (output.pEvents) {
      output.pEvents->Release();
    }
    if (output.pSample) {
      sample.Attach(output.pSample);
    }
    if (hr == MF_E_TRANSFORM_NEED_MORE_INPUT) {
      return true;
    }
    if (hr == MF_E_TRANSFORM_STREAM_CHANGE) {
      if (!SetOutputType()) {
        return false;
      }
      continue;
    }
    if (FAILED(hr) || !sample) {
      RTC_LOG(LS_WARNING) << "H.264 decoder ProcessOutput failed: "
                          << mf::HresultToString(hr)
                          << "; handing over to FFmpeg";
      return false;
    }
    if (!Deliver(sample.Get())) {
      return false;
    }
  }
}

bool MediaFoundationH264DecoderImpl::Deliver(IMFSample* sample) {
  LONGLONG time = 0;
  sample->GetSampleTime(&time);
  auto match = std::find_if(
      pending_.begin(), pending_.end(),
      [&](const Pending& frame) { return frame.sample_time == time; });
  if (match == pending_.end()) {
    if (pending_.empty()) {
      return true;
    }
    match = pending_.begin();
  }
  const Pending frame = *match;
  pending_.erase(pending_.begin(), match + 1);

  mf::ComPtr<IMFMediaBuffer> buffer;
  mf::ComPtr<IMFDXGIBuffer> dxgi;
  mf::ComPtr<ID3D11Texture2D> texture;
  UINT subresource = 0;
  if (FAILED(sample->GetBufferByIndex(0, &buffer)) ||
      FAILED(buffer.As(&dxgi)) ||
      FAILED(dxgi->GetResource(IID_PPV_ARGS(&texture))) ||
      FAILED(dxgi->GetSubresourceIndex(&subresource))) {
    RTC_LOG(LS_WARNING) << "H.264 decoder output is not a D3D11 surface; "
                           "handing over to FFmpeg";
    return false;
  }

  scoped_refptr<I420Buffer> i420 =
      buffer_pool_.CreateI420Buffer(width_, height_);
  if (!i420) {
    return true;
  }
  if (!ReadBack(texture.Get(), subresource, i420->MutableDataY(),
                i420->StrideY(), i420->MutableDataU(), i420->StrideU(),
                i420->MutableDataV(), i420->StrideV())) {
    return false;
  }
  if (!reported_) {
    reported_ = true;
    RTC_LOG(LS_INFO) << "H.264 decoded in hardware through D3D11 at " << width_
                     << "x" << height_;
  }

  VideoFrame decoded = VideoFrame::Builder()
                           .set_video_frame_buffer(i420)
                           .set_timestamp_rtp(frame.rtp_timestamp)
                           .set_color_space(frame.color_space)
                           .build();
  const int32_t decode_ms =
      static_cast<int32_t>((TimeMicros() - frame.submitted_us) / 1000);
  std::optional<uint8_t> qp;
  if (std::optional<int> last = parser_.GetLastSliceQp()) {
    qp = static_cast<uint8_t>(*last);
  }
  callback_->Decoded(decoded, decode_ms, qp);
  return true;
}

bool MediaFoundationH264DecoderImpl::ReadBack(ID3D11Texture2D* texture,
                                              UINT subresource,
                                              uint8_t* y,
                                              int stride_y,
                                              uint8_t* u,
                                              int stride_u,
                                              uint8_t* v,
                                              int stride_v) {
  D3D11_TEXTURE2D_DESC desc;
  texture->GetDesc(&desc);
  if (desc.Format != DXGI_FORMAT_NV12 ||
      static_cast<int>(desc.Width) < width_ ||
      static_cast<int>(desc.Height) < height_) {
    return false;
  }
  D3D11_TEXTURE2D_DESC staging_desc = {};
  if (staging_) {
    staging_->GetDesc(&staging_desc);
  }
  if (!staging_ || staging_desc.Width != desc.Width ||
      staging_desc.Height != desc.Height) {
    staging_.Reset();
    staging_desc = {};
    staging_desc.Width = desc.Width;
    staging_desc.Height = desc.Height;
    staging_desc.MipLevels = 1;
    staging_desc.ArraySize = 1;
    staging_desc.Format = DXGI_FORMAT_NV12;
    staging_desc.SampleDesc.Count = 1;
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    if (FAILED(
            d3d_->device->CreateTexture2D(&staging_desc, nullptr, &staging_))) {
      return false;
    }
  }

  context_->CopySubresourceRegion(staging_.Get(), 0, 0, 0, 0, texture,
                                  subresource, nullptr);
  // Waits for the copy on an event rather than in Map, where drivers spin.
  if (fence_ && context4_) {
    ++fence_value_;
    if (SUCCEEDED(context4_->Signal(fence_.Get(), fence_value_)) &&
        SUCCEEDED(fence_->SetEventOnCompletion(fence_value_, fence_event_))) {
      context4_->Flush();
      WaitForSingleObject(fence_event_, 100);
    }
  }
  D3D11_MAPPED_SUBRESOURCE mapped;
  if (FAILED(context_->Map(staging_.Get(), 0, D3D11_MAP_READ, 0, &mapped))) {
    return false;
  }
  const uint8_t* src_y = static_cast<const uint8_t*>(mapped.pData);
  const uint8_t* src_uv = src_y + mapped.RowPitch * desc.Height;
  libyuv::NV12ToI420(src_y, mapped.RowPitch, src_uv, mapped.RowPitch, y,
                     stride_y, u, stride_u, v, stride_v, width_, height_);
  context_->Unmap(staging_.Get(), 0);
  return true;
}

int32_t MediaFoundationH264DecoderImpl::RegisterDecodeCompleteCallback(
    DecodedImageCallback* callback) {
  callback_ = callback;
  return WEBRTC_VIDEO_CODEC_OK;
}

int32_t MediaFoundationH264DecoderImpl::Release() {
  if (transform_) {
    transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
    transform_->ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
  }
  transform_.Reset();
  staging_.Reset();
  fence_.Reset();
  context4_.Reset();
  if (fence_event_) {
    CloseHandle(fence_event_);
    fence_event_ = nullptr;
  }
  context_.Reset();
  d3d_.reset();
  pending_.clear();
  saw_key_frame_ = false;
  buffer_pool_.Release();
  return WEBRTC_VIDEO_CODEC_OK;
}

VideoDecoder::DecoderInfo MediaFoundationH264DecoderImpl::GetDecoderInfo()
    const {
  DecoderInfo info;
  info.implementation_name = "MediaFoundation (D3D11)";
  info.is_hardware_accelerated = true;
  return info;
}

}  // namespace webrtc
