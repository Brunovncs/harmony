#include "h264_encoder_impl.h"

#include <codecapi.h>
#include <mferror.h>
#include <strmif.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cmath>
#include <condition_variable>
#include <cstdio>
#include <cstring>
#include <deque>
#include <limits>
#include <mutex>
#include <optional>
#include <span>

#include "api/video/color_space.h"
#include "api/video/encoded_image.h"
#include "api/video/video_frame_buffer.h"
#include "api/video_codecs/scalability_mode.h"
#include "common_video/h264/h264_bitstream_parser.h"
#include "common_video/h264/h264_common.h"
#include "mf_common.h"
#include "modules/video_coding/codecs/h264/include/h264_globals.h"
#include "modules/video_coding/include/video_error_codes.h"
#include "rtc_base/logging.h"
#include "rtc_base/time_utils.h"
#include "third_party/libyuv/include/libyuv/convert_from.h"
#include "third_party/libyuv/include/libyuv/planar_functions.h"

namespace webrtc {
namespace mf {

namespace {

// OpenH264's thresholds; the hardware encoders report comparable QPs.
constexpr int kLowH264QpThreshold = 24;
constexpr int kHighH264QpThreshold = 37;
constexpr size_t kMaxPooledSamples = 8;
constexpr auto kCloseTimeout = std::chrono::milliseconds(500);
constexpr int kLatencyReportFrames = 600;

HRESULT SetUInt32(ICodecAPI* api, const GUID& key, UINT32 value) {
  VARIANT var = {};
  var.vt = VT_UI4;
  var.ulVal = value;
  return api->SetValue(&key, &var);
}

HRESULT SetBool(ICodecAPI* api, const GUID& key, bool value) {
  VARIANT var = {};
  var.vt = VT_BOOL;
  var.boolVal = value ? VARIANT_TRUE : VARIANT_FALSE;
  return api->SetValue(&key, &var);
}

// Media Foundation's H.264 profiles that produce a stream decodable under the
// negotiated one, in order of preference. Constrained High is High without
// B-frames, which these sessions never produce.
std::vector<UINT32> MfProfilesFor(H264Profile profile) {
  switch (profile) {
    case H264Profile::kProfileConstrainedHigh:
    case H264Profile::kProfileHigh:
      return {eAVEncH264VProfile_High};
    case H264Profile::kProfileMain:
      return {eAVEncH264VProfile_Main};
    case H264Profile::kProfileConstrainedBaseline:
    case H264Profile::kProfileBaseline:
      return {eAVEncH264VProfile_ConstrainedBase, eAVEncH264VProfile_Base};
    default:
      return {};
  }
}

}  // namespace

struct SessionConfig {
  int width = 0;
  int height = 0;
  int framerate = 30;
  uint32_t bitrate_bps = 0;
  H264Profile profile = H264Profile::kProfileConstrainedBaseline;
  bool screenshare = false;
};

struct FrameInfo {
  int64_t sample_time = 0;
  uint32_t rtp_timestamp = 0;
  int64_t ntp_time_ms = 0;
  int64_t capture_time_ms = 0;
  int64_t submitted_us = 0;
  VideoRotation rotation = kVideoRotation_0;
  std::optional<ColorSpace> color_space;
};

struct InputSample {
  ComPtr<IMFSample> sample;
  ComPtr<IMFMediaBuffer> buffer;

  explicit operator bool() const { return sample && buffer; }
};

// Lends a WebRTC frame's memory to the transform instead of copying it; the
// frame stays alive for as long as the transform holds on to the buffer.
class FrameMediaBuffer final : public IMFMediaBuffer {
 public:
  FrameMediaBuffer(scoped_refptr<VideoFrameBuffer> frame,
                   const uint8_t* data,
                   DWORD length)
      : frame_(std::move(frame)),
        data_(const_cast<BYTE*>(data)),
        length_(length) {}

  STDMETHODIMP QueryInterface(REFIID riid, void** object) override {
    if (!object) {
      return E_POINTER;
    }
    if (riid == __uuidof(IUnknown) || riid == __uuidof(IMFMediaBuffer)) {
      *object = static_cast<IMFMediaBuffer*>(this);
      AddRef();
      return S_OK;
    }
    *object = nullptr;
    return E_NOINTERFACE;
  }

  STDMETHODIMP_(ULONG) AddRef() override { return ++refs_; }

  STDMETHODIMP_(ULONG) Release() override {
    ULONG refs = --refs_;
    if (refs == 0) {
      delete this;
    }
    return refs;
  }

  STDMETHODIMP Lock(BYTE** data, DWORD* max_length, DWORD* length) override {
    if (!data) {
      return E_POINTER;
    }
    *data = data_;
    if (max_length) {
      *max_length = length_;
    }
    if (length) {
      *length = length_;
    }
    return S_OK;
  }

  STDMETHODIMP Unlock() override { return S_OK; }

  STDMETHODIMP GetCurrentLength(DWORD* length) override {
    if (!length) {
      return E_POINTER;
    }
    *length = length_;
    return S_OK;
  }

  STDMETHODIMP SetCurrentLength(DWORD length) override {
    return length == length_ ? S_OK : E_INVALIDARG;
  }

  STDMETHODIMP GetMaxLength(DWORD* length) override {
    return GetCurrentLength(length);
  }

 private:
  std::atomic<ULONG> refs_{1};
  scoped_refptr<VideoFrameBuffer> frame_;
  BYTE* data_;
  DWORD length_;
};

// An NV12 frame laid out exactly as Media Foundation's NV12 (no row padding,
// chroma right after luma) can be handed over as it is.
InputSample LendNv12(const scoped_refptr<VideoFrameBuffer>& frame) {
  const NV12BufferInterface* nv12 = frame->GetNV12();
  const int width = frame->width();
  const int height = frame->height();
  if (!nv12 || width % 2 || height % 2 || nv12->StrideY() != width ||
      nv12->StrideUV() != width ||
      nv12->DataUV() != nv12->DataY() + width * height) {
    return {};
  }
  InputSample input;
  if (FAILED(MFCreateSample(&input.sample))) {
    return {};
  }
  input.buffer.Attach(
      new FrameMediaBuffer(frame, nv12->DataY(), width * height * 3 / 2));
  if (FAILED(input.sample->AddBuffer(input.buffer.Get()))) {
    return {};
  }
  return input;
}

// One opened transform. Media Foundation reports its events on its own work
// queue threads, which also collect and deliver the encoded frames, so this is
// shared between the encoder and whichever callback is in flight.
class EncoderSession : public std::enable_shared_from_this<EncoderSession> {
 public:
  enum class Submitted { kOk, kNoSlot, kFailed };

  static std::shared_ptr<EncoderSession> Open(const MftCandidate& candidate,
                                              const SessionConfig& config);
  ~EncoderSession();

  const std::string& name() const { return candidate_.name; }
  bool failed() const { return failed_.load(); }
  bool key_frame_seen() const { return key_frame_seen_.load(); }
  void ExpectKeyFrame() { key_frame_seen_ = false; }

  void SetCallback(EncodedImageCallback* callback);
  InputSample AcquireInput();
  Submitted Submit(IMFSample* sample,
                   const FrameInfo& info,
                   bool key_frame,
                   std::chrono::milliseconds wait);
  void SetRates(uint32_t bitrate_bps);
  void Close();

 private:
  class EventCallback;

  EncoderSession(MftCandidate candidate,
                 const SessionConfig& config,
                 bool use_d3d)
      : candidate_(std::move(candidate)), config_(config), use_d3d_(use_d3d) {}

  bool Configure();
  HRESULT SetOutputType();
  bool AttachD3DManager();
  bool SetInputType();
  void ApplyCodecSettings();
  void ReadSequenceHeader();
  bool StartEvents();
  void OnEvent(IMFAsyncResult* result);
  void ProcessOutput();
  void Deliver(const uint8_t* data, size_t size, std::optional<int64_t> time);
  void Fail(const char* what, HRESULT hr);

  MftCandidate candidate_;
  const SessionConfig config_;
  // Only for transforms that insist on one.
  const bool use_d3d_;
  bool wants_d3d_ = false;
  std::optional<D3DDevice> d3d_;
  ComPtr<IMFTransform> transform_;
  ComPtr<ICodecAPI> codec_api_;
  ComPtr<IMFMediaEventGenerator> events_;
  ComPtr<IMFAsyncCallback> event_callback_;
  DWORD input_id_ = 0;
  DWORD output_id_ = 0;
  bool provides_samples_ = true;
  DWORD output_size_ = 0;
  uint32_t mean_bitrate_ = 0;

  std::mutex mutex_;
  std::condition_variable cv_;
  int need_input_ = 0;
  bool event_pending_ = false;
  bool closing_ = false;
  std::deque<FrameInfo> pending_;
  std::atomic<bool> failed_{false};
  std::atomic<bool> key_frame_seen_{false};

  // Encoder thread only.
  std::vector<InputSample> pool_;

  // Event threads only; Media Foundation never runs two callbacks of one
  // generator at once.
  std::vector<uint8_t> sequence_header_;
  H264BitstreamParser parser_;
  EncodedImage image_;
  bool reported_sps_ = false;
  bool prepended_ = false;
  int64_t last_output_time_ = -1;
  int64_t latency_sum_us_ = 0;
  int latency_frames_ = 0;

  std::mutex delivery_mutex_;
  EncodedImageCallback* callback_ = nullptr;
};

class EncoderSession::EventCallback final : public IMFAsyncCallback {
 public:
  explicit EventCallback(std::weak_ptr<EncoderSession> session)
      : session_(std::move(session)) {}

  STDMETHODIMP QueryInterface(REFIID riid, void** object) override {
    if (!object) {
      return E_POINTER;
    }
    if (riid == __uuidof(IUnknown) || riid == __uuidof(IMFAsyncCallback)) {
      *object = static_cast<IMFAsyncCallback*>(this);
      AddRef();
      return S_OK;
    }
    *object = nullptr;
    return E_NOINTERFACE;
  }

  STDMETHODIMP_(ULONG) AddRef() override { return ++refs_; }

  STDMETHODIMP_(ULONG) Release() override {
    ULONG refs = --refs_;
    if (refs == 0) {
      delete this;
    }
    return refs;
  }

  STDMETHODIMP GetParameters(DWORD*, DWORD*) override { return E_NOTIMPL; }

  STDMETHODIMP Invoke(IMFAsyncResult* result) override {
    if (auto session = session_.lock()) {
      session->OnEvent(result);
    }
    return S_OK;
  }

 private:
  std::atomic<ULONG> refs_{1};
  std::weak_ptr<EncoderSession> session_;
};

std::shared_ptr<EncoderSession> EncoderSession::Open(
    const MftCandidate& candidate,
    const SessionConfig& config) {
  // AMD's transform takes no media type until it has a D3D11 device, even
  // for system-memory input, and no device once it has refused a type; so it
  // is opened a second time with one. NVIDIA's does without.
  for (bool use_d3d : {false, true}) {
    std::shared_ptr<EncoderSession> session(
        new EncoderSession(candidate, config, use_d3d));
    if (session->Configure()) {
      return session;
    }
    const bool retry = !use_d3d && session->wants_d3d_;
    session->Close();
    if (!retry) {
      break;
    }
  }
  return nullptr;
}

EncoderSession::~EncoderSession() {
  Close();
}

bool EncoderSession::Configure() {
  HRESULT hr = candidate_.activate->ActivateObject(IID_PPV_ARGS(&transform_));
  if (FAILED(hr)) {
    RTC_LOG(LS_WARNING) << name() << ": activation failed, "
                        << HresultToString(hr);
    return false;
  }

  ComPtr<IMFAttributes> attributes;
  if (FAILED(transform_->GetAttributes(&attributes))) {
    return false;
  }
  UINT32 is_async = 0;
  attributes->GetUINT32(MF_TRANSFORM_ASYNC, &is_async);
  if (!is_async) {
    RTC_LOG(LS_WARNING) << name() << " is synchronous; only asynchronous "
                        << "hardware transforms are driven here";
    return false;
  }
  hr = attributes->SetUINT32(MF_TRANSFORM_ASYNC_UNLOCK, TRUE);
  if (FAILED(hr)) {
    RTC_LOG(LS_WARNING) << name() << ": unlock failed, " << HresultToString(hr);
    return false;
  }
  attributes->SetUINT32(MF_LOW_LATENCY, TRUE);
  UINT32 d3d11_aware = 0;
  attributes->GetUINT32(MF_SA_D3D11_AWARE, &d3d11_aware);

  if (FAILED(transform_.As(&codec_api_)) || FAILED(transform_.As(&events_))) {
    RTC_LOG(LS_WARNING) << name() << " lacks ICodecAPI or events";
    return false;
  }

  DWORD inputs = 0;
  DWORD outputs = 0;
  if (FAILED(transform_->GetStreamCount(&inputs, &outputs)) || inputs != 1 ||
      outputs != 1) {
    return false;
  }
  hr = transform_->GetStreamIDs(1, &input_id_, 1, &output_id_);
  if (hr == E_NOTIMPL) {
    input_id_ = 0;
    output_id_ = 0;
  } else if (FAILED(hr)) {
    return false;
  }

  if (use_d3d_ && !AttachD3DManager()) {
    return false;
  }

  // Some transforms only take rate control and GOP settings before the media
  // types, others only after; set them both times.
  ApplyCodecSettings();

  hr = SetOutputType();
  if (hr == MF_E_UNSUPPORTED_D3D_TYPE && d3d11_aware && !d3d_) {
    wants_d3d_ = true;
    return false;
  }
  if (FAILED(hr) || !SetInputType()) {
    return false;
  }
  ApplyCodecSettings();

  MFT_OUTPUT_STREAM_INFO info = {};
  if (FAILED(transform_->GetOutputStreamInfo(output_id_, &info))) {
    return false;
  }
  provides_samples_ = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES |
                                      MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES);
  output_size_ =
      std::max<DWORD>(info.cbSize, config_.width * config_.height * 3 / 2);
  ReadSequenceHeader();

  transform_->ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
  hr = transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
  if (SUCCEEDED(hr)) {
    hr = transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
  }
  if (FAILED(hr)) {
    RTC_LOG(LS_WARNING) << name() << ": start of stream failed, "
                        << HresultToString(hr);
    return false;
  }
  if (!StartEvents()) {
    return false;
  }

  RTC_LOG(LS_INFO) << "Media Foundation H.264 encoder " << name() << " ("
                   << candidate_.vendor_id
                   << (d3d_ ? ", D3D11 device" : ", system memory")
                   << ") opened at " << config_.width << "x" << config_.height
                   << "@" << config_.framerate << ", "
                   << config_.bitrate_bps / 1000 << " kbps";
  return true;
}

HRESULT EncoderSession::SetOutputType() {
  HRESULT hr = MF_E_INVALIDMEDIATYPE;
  for (UINT32 mf_profile : MfProfilesFor(config_.profile)) {
    ComPtr<IMFMediaType> type;
    hr = MFCreateMediaType(&type);
    if (FAILED(hr)) {
      return hr;
    }
    type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
    type->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_H264);
    type->SetUINT32(MF_MT_AVG_BITRATE, config_.bitrate_bps);
    type->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
    type->SetUINT32(MF_MT_MPEG2_PROFILE, mf_profile);
    MFSetAttributeSize(type.Get(), MF_MT_FRAME_SIZE, config_.width,
                       config_.height);
    MFSetAttributeRatio(type.Get(), MF_MT_FRAME_RATE, config_.framerate, 1);
    MFSetAttributeRatio(type.Get(), MF_MT_PIXEL_ASPECT_RATIO, 1, 1);
    hr = transform_->SetOutputType(output_id_, type.Get(), 0);
    if (SUCCEEDED(hr)) {
      return hr;
    }
    RTC_LOG(LS_INFO) << name() << " refused H.264 profile " << mf_profile
                     << ": " << HresultToString(hr);
  }
  return hr;
}

bool EncoderSession::AttachD3DManager() {
  std::optional<D3DDevice> d3d = CreateD3DDevice(candidate_.adapter_luid);
  if (!d3d) {
    return false;
  }
  HRESULT hr = transform_->ProcessMessage(
      MFT_MESSAGE_SET_D3D_MANAGER,
      reinterpret_cast<ULONG_PTR>(d3d->manager.Get()));
  if (FAILED(hr)) {
    RTC_LOG(LS_WARNING) << name()
                        << " refused a D3D11 device: " << HresultToString(hr);
    return false;
  }
  d3d_ = std::move(*d3d);
  return true;
}

bool EncoderSession::SetInputType() {
  ComPtr<IMFMediaType> type;
  if (FAILED(MFCreateMediaType(&type))) {
    return false;
  }
  type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
  type->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_NV12);
  type->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
  MFSetAttributeSize(type.Get(), MF_MT_FRAME_SIZE, config_.width,
                     config_.height);
  MFSetAttributeRatio(type.Get(), MF_MT_FRAME_RATE, config_.framerate, 1);
  MFSetAttributeRatio(type.Get(), MF_MT_PIXEL_ASPECT_RATIO, 1, 1);
  HRESULT hr = transform_->SetInputType(input_id_, type.Get(), 0);
  if (FAILED(hr)) {
    RTC_LOG(LS_WARNING) << name() << " refused NV12 input at " << config_.width
                        << "x" << config_.height << ": " << HresultToString(hr);
    return false;
  }
  return true;
}

void EncoderSession::ApplyCodecSettings() {
  SetBool(codec_api_.Get(), CODECAPI_AVLowLatencyMode, true);
  // WebRTC has no way to carry B-frames: output order must be input order.
  SetUInt32(codec_api_.Get(), CODECAPI_AVEncMPVDefaultBPictureCount, 0);
  SetUInt32(codec_api_.Get(), CODECAPI_AVEncCommonRateControlMode,
            eAVEncCommonRateControlMode_CBR);
  SetUInt32(codec_api_.Get(), CODECAPI_AVEncCommonMeanBitRate,
            mean_bitrate_ ? mean_bitrate_ : config_.bitrate_bps);
  // Key frames only when WebRTC asks for one (a new viewer, a lost packet):
  // ten minutes, as some transforms read 0 as their own short default.
  SetUInt32(codec_api_.Get(), CODECAPI_AVEncMPVGOPSize,
            static_cast<UINT32>(config_.framerate) * 600);
}

void EncoderSession::ReadSequenceHeader() {
  ComPtr<IMFMediaType> type;
  if (FAILED(transform_->GetOutputCurrentType(output_id_, &type))) {
    return;
  }
  UINT32 size = 0;
  if (FAILED(type->GetBlobSize(MF_MT_MPEG_SEQUENCE_HEADER, &size)) ||
      size == 0) {
    return;
  }
  std::vector<uint8_t> header(size);
  if (SUCCEEDED(type->GetBlob(MF_MT_MPEG_SEQUENCE_HEADER, header.data(), size,
                              nullptr))) {
    sequence_header_ = std::move(header);
  }
}

bool EncoderSession::StartEvents() {
  event_callback_.Attach(new EventCallback(weak_from_this()));
  std::lock_guard<std::mutex> lock(mutex_);
  HRESULT hr = events_->BeginGetEvent(event_callback_.Get(), nullptr);
  if (FAILED(hr)) {
    RTC_LOG(LS_WARNING) << name() << ": BeginGetEvent failed, "
                        << HresultToString(hr);
    return false;
  }
  event_pending_ = true;
  return true;
}

void EncoderSession::OnEvent(IMFAsyncResult* result) {
  ComPtr<IMFMediaEvent> event;
  HRESULT hr = events_->EndGetEvent(result, &event);
  MediaEventType type = MEUnknown;
  HRESULT status = S_OK;
  if (SUCCEEDED(hr)) {
    event->GetType(&type);
    event->GetStatus(&status);
  }

  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (closing_ || FAILED(hr)) {
      event_pending_ = false;
      cv_.notify_all();
      if (!closing_) {
        failed_ = true;
        RTC_LOG(LS_WARNING)
            << name() << ": EndGetEvent failed, " << HresultToString(hr);
      }
      return;
    }
  }

  if (FAILED(status)) {
    Fail("event", status);
  } else if (type == METransformNeedInput) {
    std::lock_guard<std::mutex> lock(mutex_);
    ++need_input_;
    cv_.notify_all();
  } else if (type == METransformHaveOutput) {
    ProcessOutput();
  } else if (type == MEError) {
    Fail("MEError", status);
  }

  std::lock_guard<std::mutex> lock(mutex_);
  if (closing_ || failed_) {
    event_pending_ = false;
    cv_.notify_all();
    return;
  }
  hr = events_->BeginGetEvent(event_callback_.Get(), nullptr);
  if (FAILED(hr)) {
    event_pending_ = false;
    failed_ = true;
    cv_.notify_all();
    RTC_LOG(LS_WARNING) << name() << ": BeginGetEvent failed, "
                        << HresultToString(hr);
  }
}

void EncoderSession::ProcessOutput() {
  MFT_OUTPUT_DATA_BUFFER output = {};
  output.dwStreamID = output_id_;
  ComPtr<IMFSample> sample;
  if (!provides_samples_) {
    ComPtr<IMFMediaBuffer> buffer;
    if (FAILED(MFCreateSample(&sample)) ||
        FAILED(MFCreateMemoryBuffer(output_size_, &buffer)) ||
        FAILED(sample->AddBuffer(buffer.Get()))) {
      Fail("output allocation", E_OUTOFMEMORY);
      return;
    }
    output.pSample = sample.Get();
  }

  DWORD status = 0;
  HRESULT hr = transform_->ProcessOutput(0, 1, &output, &status);
  if (output.pEvents) {
    output.pEvents->Release();
  }
  if (provides_samples_ && output.pSample) {
    sample.Attach(output.pSample);
  }

  if (hr == MF_E_TRANSFORM_STREAM_CHANGE) {
    ComPtr<IMFMediaType> type;
    hr = transform_->GetOutputAvailableType(output_id_, 0, &type);
    if (SUCCEEDED(hr)) {
      hr = transform_->SetOutputType(output_id_, type.Get(), 0);
    }
    if (FAILED(hr)) {
      Fail("output renegotiation", hr);
    } else {
      ReadSequenceHeader();
    }
    return;
  }
  if (hr == MF_E_TRANSFORM_NEED_MORE_INPUT) {
    return;
  }
  if (FAILED(hr) || !sample) {
    Fail("ProcessOutput", hr);
    return;
  }

  std::optional<int64_t> time;
  LONGLONG sample_time = 0;
  if (SUCCEEDED(sample->GetSampleTime(&sample_time))) {
    time = sample_time;
  }

  ComPtr<IMFMediaBuffer> buffer;
  if (FAILED(sample->ConvertToContiguousBuffer(&buffer))) {
    Fail("ConvertToContiguousBuffer", E_FAIL);
    return;
  }
  BYTE* data = nullptr;
  DWORD length = 0;
  if (FAILED(buffer->Lock(&data, nullptr, &length))) {
    Fail("output Lock", E_FAIL);
    return;
  }
  Deliver(data, length, time);
  buffer->Unlock();
}

void EncoderSession::Deliver(const uint8_t* data,
                             size_t size,
                             std::optional<int64_t> time) {
  // Frames out of input order mean B-frames, which RTP cannot carry.
  if (time) {
    if (*time < last_output_time_) {
      Fail("output order", E_UNEXPECTED);
      return;
    }
    last_output_time_ = *time;
  }
  FrameInfo info;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (pending_.empty()) {
      RTC_LOG(LS_WARNING) << name() << " produced a frame nobody sent";
      return;
    }
    // Inputs before the matching one were dropped by the transform. One
    // that rewrites timestamps is matched first in, first out instead.
    auto match = pending_.end();
    if (time) {
      match = std::find_if(
          pending_.begin(), pending_.end(),
          [&](const FrameInfo& frame) { return frame.sample_time == *time; });
    }
    if (match == pending_.end()) {
      match = pending_.begin();
    }
    info = *match;
    pending_.erase(pending_.begin(), match + 1);
  }

  std::span<const uint8_t> bitstream(data, size);
  bool idr = false;
  bool sps = false;
  std::vector<H264::NaluIndex> nalus = H264::FindNaluIndices(bitstream);
  for (const H264::NaluIndex& nalu : nalus) {
    const uint8_t* payload = data + nalu.payload_start_offset;
    H264::NaluType type = H264::ParseNaluType(payload[0]);
    idr |= type == H264::NaluType::kIdr;
    sps |= type == H264::NaluType::kSps;
    if (type == H264::NaluType::kSps && !reported_sps_ &&
        nalu.payload_size >= 4) {
      reported_sps_ = true;
      char profile[7];
      std::snprintf(profile, sizeof(profile), "%02x%02x%02x", payload[1],
                    payload[2], payload[3]);
      RTC_LOG(LS_INFO) << name() << " sends profile-level-id " << profile;
    }
  }
  if (nalus.empty()) {
    Fail("output is not an Annex B stream", E_UNEXPECTED);
    return;
  }
  if (idr && !sps && sequence_header_.empty()) {
    ReadSequenceHeader();
  }
  const bool prepend = idr && !sps && !sequence_header_.empty();
  if (idr && !sps && !prepended_) {
    prepended_ = true;
    RTC_LOG(LS_INFO) << name() << " sends key frames without SPS/PPS; "
                     << (prepend ? "adding" : "has no")
                     << " the sequence header";
  }

  const size_t total = size + (prepend ? sequence_header_.size() : 0);
  auto encoded = EncodedImageBuffer::Create(total);
  uint8_t* out = encoded->data();
  if (prepend) {
    std::memcpy(out, sequence_header_.data(), sequence_header_.size());
    out += sequence_header_.size();
  }
  std::memcpy(out, data, size);

  image_.SetEncodedData(encoded);
  image_.set_size(total);
  image_._encodedWidth = config_.width;
  image_._encodedHeight = config_.height;
  image_.SetRtpTimestamp(info.rtp_timestamp);
  image_.SetSimulcastIndex(0);
  image_.ntp_time_ms_ = info.ntp_time_ms;
  image_.capture_time_ms_ = info.capture_time_ms;
  image_.rotation_ = info.rotation;
  image_.content_type_ = config_.screenshare ? VideoContentType::SCREENSHARE
                                             : VideoContentType::UNSPECIFIED;
  image_.timing_.flags = VideoSendTiming::kInvalid;
  image_._frameType =
      idr ? VideoFrameType::kVideoFrameKey : VideoFrameType::kVideoFrameDelta;
  image_.SetColorSpace(info.color_space);
  parser_.ParseBitstream(std::span<const uint8_t>(encoded->data(), total));
  image_.qp_ = parser_.GetLastSliceQp().value_or(-1);

  CodecSpecificInfo codec_info;
  codec_info.codecType = kVideoCodecH264;
  codec_info.codecSpecific.H264.packetization_mode =
      H264PacketizationMode::NonInterleaved;
  codec_info.codecSpecific.H264.temporal_idx = kNoTemporalIdx;
  codec_info.codecSpecific.H264.idr_frame = idr;
  codec_info.codecSpecific.H264.base_layer_sync = false;

  if (idr) {
    key_frame_seen_ = true;
  }

  latency_sum_us_ += TimeMicros() - info.submitted_us;
  if (++latency_frames_ == kLatencyReportFrames) {
    RTC_LOG(LS_INFO) << name() << ": " << latency_sum_us_ / latency_frames_
                     << " us from input to output over the last "
                     << latency_frames_ << " frames";
    latency_sum_us_ = 0;
    latency_frames_ = 0;
  }

  std::lock_guard<std::mutex> lock(delivery_mutex_);
  if (callback_) {
    auto result = callback_->OnEncodedImage(image_, &codec_info);
    if (result.error != EncodedImageCallback::Result::OK) {
      RTC_LOG(LS_WARNING) << name() << ": OnEncodedImage failed, "
                          << result.error;
    }
  }
}

void EncoderSession::Fail(const char* what, HRESULT hr) {
  if (!failed_.exchange(true)) {
    RTC_LOG(LS_WARNING) << name() << ": " << what << " failed, "
                        << HresultToString(hr)
                        << "; handing over to the software encoder";
  }
  std::lock_guard<std::mutex> lock(mutex_);
  cv_.notify_all();
}

void EncoderSession::SetCallback(EncodedImageCallback* callback) {
  std::lock_guard<std::mutex> lock(delivery_mutex_);
  callback_ = callback;
}

InputSample EncoderSession::AcquireInput() {
  // A sample is free again once the transform has let go of it: then the
  // pool holds the only reference to the sample, and the sample and the pool
  // the only two to its buffer.
  for (InputSample& input : pool_) {
    input.sample->AddRef();
    ULONG sample_refs = input.sample->Release();
    input.buffer->AddRef();
    ULONG buffer_refs = input.buffer->Release();
    if (sample_refs == 1 && buffer_refs == 2) {
      return input;
    }
  }

  const DWORD size = config_.width * config_.height +
                     2 * ((config_.width + 1) / 2) * ((config_.height + 1) / 2);
  InputSample input;
  if (FAILED(MFCreateSample(&input.sample)) ||
      FAILED(MFCreateAlignedMemoryBuffer(size, MF_64_BYTE_ALIGNMENT,
                                         &input.buffer)) ||
      FAILED(input.sample->AddBuffer(input.buffer.Get()))) {
    return {};
  }
  if (pool_.size() < kMaxPooledSamples) {
    pool_.push_back(input);
  }
  return input;
}

EncoderSession::Submitted EncoderSession::Submit(
    IMFSample* sample,
    const FrameInfo& info,
    bool key_frame,
    std::chrono::milliseconds wait) {
  {
    std::unique_lock<std::mutex> lock(mutex_);
    if (!cv_.wait_for(lock, wait,
                      [&] { return need_input_ > 0 || failed_ || closing_; })) {
      return Submitted::kNoSlot;
    }
    if (failed_ || closing_) {
      return Submitted::kFailed;
    }
    --need_input_;
    pending_.push_back(info);
  }

  if (key_frame) {
    HRESULT hr =
        SetUInt32(codec_api_.Get(), CODECAPI_AVEncVideoForceKeyFrame, 1);
    if (FAILED(hr)) {
      Fail("forcing a key frame", hr);
      return Submitted::kFailed;
    }
  }

  HRESULT hr = transform_->ProcessInput(input_id_, sample, 0);
  if (SUCCEEDED(hr)) {
    return Submitted::kOk;
  }
  {
    std::lock_guard<std::mutex> lock(mutex_);
    auto it = std::find_if(pending_.begin(), pending_.end(),
                           [&](const FrameInfo& frame) {
                             return frame.sample_time == info.sample_time;
                           });
    if (it != pending_.end()) {
      pending_.erase(it);
    }
  }
  if (hr == MF_E_NOTACCEPTING) {
    return Submitted::kNoSlot;
  }
  Fail("ProcessInput", hr);
  return Submitted::kFailed;
}

void EncoderSession::SetRates(uint32_t bitrate_bps) {
  if (bitrate_bps == mean_bitrate_) {
    return;
  }
  HRESULT hr =
      SetUInt32(codec_api_.Get(), CODECAPI_AVEncCommonMeanBitRate, bitrate_bps);
  if (FAILED(hr) && mean_bitrate_ == 0) {
    RTC_LOG(LS_WARNING) << name() << " does not take bitrate changes: "
                        << HresultToString(hr);
  }
  mean_bitrate_ = bitrate_bps;
}

void EncoderSession::Close() {
  {
    std::lock_guard<std::mutex> lock(delivery_mutex_);
    callback_ = nullptr;
  }
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (closing_) {
      return;
    }
    closing_ = true;
    cv_.notify_all();
  }
  if (transform_) {
    transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
    ComPtr<IMFShutdown> shutdown;
    if (SUCCEEDED(transform_.As(&shutdown))) {
      shutdown->Shutdown();
    }
  }
  {
    // Shutting the transform down completes the outstanding event request;
    // a transform that never does is left to the weak reference.
    std::unique_lock<std::mutex> lock(mutex_);
    cv_.wait_for(lock, kCloseTimeout, [&] { return !event_pending_; });
  }
  pool_.clear();
  if (candidate_.activate) {
    candidate_.activate->ShutdownObject();
  }
}

}  // namespace mf

namespace {

// A transform with no input slot for this long is considered hung.
constexpr int64_t kStallLimitMs = 2000;
// Frames to wait for the key frame a request should have produced.
constexpr int kKeyFrameDeadlineFrames = 60;

}  // namespace

MediaFoundationH264EncoderImpl::MediaFoundationH264EncoderImpl(
    const Environment& env,
    const SdpVideoFormat& format)
    : env_(env) {
  if (auto profile_level_id =
          ParseSdpForH264ProfileLevelId(format.parameters)) {
    profile_ = profile_level_id->profile;
  }
}

MediaFoundationH264EncoderImpl::~MediaFoundationH264EncoderImpl() {
  CloseSession();
}

int32_t MediaFoundationH264EncoderImpl::InitEncode(
    const VideoCodec* codec_settings,
    const Settings& settings) {
  if (!codec_settings || codec_settings->codecType != kVideoCodecH264 ||
      codec_settings->width < 2 || codec_settings->height < 2 ||
      codec_settings->maxFramerate == 0) {
    return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
  }
  if (codec_settings->numberOfSimulcastStreams > 1) {
    return WEBRTC_VIDEO_CODEC_ERR_SIMULCAST_PARAMETERS_NOT_SUPPORTED;
  }
  // Temporal layers are OpenH264's to provide.
  std::optional<ScalabilityMode> mode = codec_settings->GetScalabilityMode();
  if (mode && *mode != ScalabilityMode::kL1T1) {
    return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
  }

  CloseSession();
  codec_ = *codec_settings;
  target_bps_ = codec_.startBitrate * 1000;
  framerate_ = codec_.maxFramerate;
  sending_ = false;
  key_frame_pending_ = true;
  awaiting_key_frame_ = false;
  stalled_since_ms_ = -1;
  return OpenSession(codec_.width, codec_.height);
}

int32_t MediaFoundationH264EncoderImpl::OpenSession(int width, int height) {
  if (!mf::EnsureComInitialized()) {
    return WEBRTC_VIDEO_CODEC_ERROR;
  }
  mf::SessionConfig config;
  config.width = width;
  config.height = height;
  config.framerate = std::max(1, static_cast<int>(codec_.maxFramerate));
  config.bitrate_bps = std::max<uint32_t>(target_bps_, 300'000);
  config.profile = profile_;
  config.screenshare = codec_.mode == VideoCodecMode::kScreensharing;

  for (const mf::MftCandidate& candidate :
       mf::EnumerateHardwareMfts(MFT_CATEGORY_VIDEO_ENCODER, MFVideoFormat_NV12,
                                 MFVideoFormat_H264, mf::kEncoderOverrideEnv)) {
    if (auto session = mf::EncoderSession::Open(candidate, config)) {
      session->SetCallback(callback_);
      implementation_name_ = "MediaFoundation (" + session->name() + ")";
      session_ = std::move(session);
      return WEBRTC_VIDEO_CODEC_OK;
    }
  }
  RTC_LOG(LS_WARNING) << "No hardware H.264 Media Foundation encoder opened "
                      << "at " << width << "x" << height;
  return WEBRTC_VIDEO_CODEC_ERROR;
}

void MediaFoundationH264EncoderImpl::CloseSession() {
  if (session_) {
    session_->Close();
    session_.reset();
  }
}

int32_t MediaFoundationH264EncoderImpl::RegisterEncodeCompleteCallback(
    EncodedImageCallback* callback) {
  callback_ = callback;
  if (session_) {
    session_->SetCallback(callback);
  }
  return WEBRTC_VIDEO_CODEC_OK;
}

int32_t MediaFoundationH264EncoderImpl::Release() {
  CloseSession();
  return WEBRTC_VIDEO_CODEC_OK;
}

int32_t MediaFoundationH264EncoderImpl::Encode(
    const VideoFrame& frame,
    const std::vector<VideoFrameType>* frame_types) {
  if (!session_ || !callback_) {
    return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
  }
  if (session_->failed()) {
    return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
  }
  if (!sending_) {
    return WEBRTC_VIDEO_CODEC_NO_OUTPUT;
  }
  if (frame_types && !frame_types->empty()) {
    if ((*frame_types)[0] == VideoFrameType::kEmptyFrame) {
      return WEBRTC_VIDEO_CODEC_NO_OUTPUT;
    }
    if ((*frame_types)[0] == VideoFrameType::kVideoFrameKey) {
      key_frame_pending_ = true;
    }
  }

  scoped_refptr<VideoFrameBuffer> buffer = frame.video_frame_buffer();
  const int width = buffer->width();
  const int height = buffer->height();
  if (width != static_cast<int>(codec_.width) ||
      height != static_cast<int>(codec_.height)) {
    // InitEncode normally comes first with a new size; should a frame beat
    // it, follow the frame.
    CloseSession();
    codec_.width = width;
    codec_.height = height;
    key_frame_pending_ = true;
    if (OpenSession(width, height) != WEBRTC_VIDEO_CODEC_OK) {
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    }
  }

  mf::InputSample input;
  if (buffer->type() == VideoFrameBuffer::Type::kNV12) {
    input = mf::LendNv12(buffer);
  }
  if (!input) {
    input = session_->AcquireInput();
    if (!input) {
      return WEBRTC_VIDEO_CODEC_MEMORY;
    }
    BYTE* data = nullptr;
    if (FAILED(input.buffer->Lock(&data, nullptr, nullptr))) {
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    }
    const int uv_stride = 2 * ((width + 1) / 2);
    uint8_t* dst_y = data;
    uint8_t* dst_uv = data + width * height;
    if (buffer->type() == VideoFrameBuffer::Type::kNV12) {
      const NV12BufferInterface* nv12 = buffer->GetNV12();
      libyuv::CopyPlane(nv12->DataY(), nv12->StrideY(), dst_y, width, width,
                        height);
      libyuv::CopyPlane(nv12->DataUV(), nv12->StrideUV(), dst_uv, uv_stride,
                        uv_stride, (height + 1) / 2);
    } else {
      scoped_refptr<I420BufferInterface> i420 = buffer->ToI420();
      if (!i420) {
        input.buffer->Unlock();
        RTC_LOG(LS_ERROR) << "Cannot convert "
                          << VideoFrameBufferTypeToString(buffer->type())
                          << " to I420 for the hardware encoder";
        return WEBRTC_VIDEO_CODEC_ENCODER_FAILURE;
      }
      libyuv::I420ToNV12(i420->DataY(), i420->StrideY(), i420->DataU(),
                         i420->StrideU(), i420->DataV(), i420->StrideV(), dst_y,
                         width, dst_uv, uv_stride, width, height);
    }
    input.buffer->Unlock();
    input.buffer->SetCurrentLength(width * height +
                                   uv_stride * ((height + 1) / 2));
  }

  // 100 ns units, strictly increasing: the transform hands the time back
  // with the encoded frame, which is how it is matched to its RTP timestamp.
  int64_t sample_time = frame.timestamp_us() * 10;
  if (sample_time <= last_sample_time_) {
    sample_time = last_sample_time_ + 1;
  }
  last_sample_time_ = sample_time;
  input.sample->SetSampleTime(sample_time);
  input.sample->SetSampleDuration(
      static_cast<LONGLONG>(10'000'000 / std::max(1.0, framerate_)));

  mf::FrameInfo info;
  info.sample_time = sample_time;
  info.rtp_timestamp = frame.rtp_timestamp();
  info.ntp_time_ms = frame.ntp_time_ms();
  info.capture_time_ms = frame.render_time_ms();
  info.submitted_us = TimeMicros();
  info.rotation = frame.rotation();
  info.color_space = frame.color_space();

  const bool key_frame = key_frame_pending_;
  if (key_frame) {
    // Before submitting: the key frame may come back before Submit returns.
    session_->ExpectKeyFrame();
  }
  // Up to a frame interval for an input slot: hardware transforms free one
  // within a millisecond or two, so a longer wait means the frame is dropped
  // rather than queued behind the next one.
  const auto wait = std::chrono::milliseconds(
      std::clamp(static_cast<int>(1000 / std::max(1.0, framerate_)), 5, 100));
  switch (session_->Submit(input.sample.Get(), info, key_frame, wait)) {
    case mf::EncoderSession::Submitted::kOk:
      break;
    case mf::EncoderSession::Submitted::kNoSlot: {
      const int64_t now = env_.clock().TimeInMilliseconds();
      if (stalled_since_ms_ < 0) {
        stalled_since_ms_ = now;
      } else if (now - stalled_since_ms_ > kStallLimitMs) {
        RTC_LOG(LS_WARNING) << implementation_name_ << " stopped taking "
                            << "frames; handing over to the software encoder";
        return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
      }
      return WEBRTC_VIDEO_CODEC_OK;
    }
    case mf::EncoderSession::Submitted::kFailed:
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
  }
  stalled_since_ms_ = -1;

  if (key_frame) {
    key_frame_pending_ = false;
    awaiting_key_frame_ = true;
    frames_since_key_request_ = 0;
  } else if (awaiting_key_frame_) {
    if (session_->key_frame_seen()) {
      awaiting_key_frame_ = false;
    } else if (++frames_since_key_request_ > kKeyFrameDeadlineFrames) {
      RTC_LOG(LS_WARNING) << implementation_name_ << " ignored a key frame "
                          << "request; handing over to the software encoder";
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    }
  }
  return WEBRTC_VIDEO_CODEC_OK;
}

void MediaFoundationH264EncoderImpl::SetRates(
    const RateControlParameters& parameters) {
  if (!std::isfinite(parameters.framerate_fps) ||
      parameters.framerate_fps < 1.0) {
    return;
  }
  target_bps_ = parameters.bitrate.get_sum_bps();
  framerate_ = parameters.framerate_fps;
  if (target_bps_ == 0) {
    sending_ = false;
    return;
  }
  if (!sending_) {
    // A stream that starts or resumes needs a key frame.
    key_frame_pending_ = true;
    sending_ = true;
  }
  if (session_) {
    session_->SetRates(target_bps_);
  }
}

VideoEncoder::EncoderInfo MediaFoundationH264EncoderImpl::GetEncoderInfo()
    const {
  EncoderInfo info;
  info.supports_native_handle = false;
  info.implementation_name = implementation_name_;
  info.scaling_settings = VideoEncoder::ScalingSettings(
      mf::kLowH264QpThreshold, mf::kHighH264QpThreshold);
  info.requested_resolution_alignment = 2;
  info.is_hardware_accelerated = true;
  info.supports_simulcast = false;
  info.is_qp_trusted = true;
  info.preferred_pixel_formats = {VideoFrameBuffer::Type::kNV12};
  return info;
}

}  // namespace webrtc
