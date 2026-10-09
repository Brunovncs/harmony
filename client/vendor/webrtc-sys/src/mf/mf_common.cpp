#include "mf_common.h"

#include <d3d10.h>
#include <dxgi.h>
#include <objbase.h>

#include <algorithm>
#include <cctype>
#include <cstdio>
#include <cstdlib>

#include "rtc_base/logging.h"

namespace webrtc {
namespace mf {

namespace {

std::string ToUtf8(const wchar_t* text, int length) {
  if (!text || length <= 0) {
    return {};
  }
  int size = WideCharToMultiByte(CP_UTF8, 0, text, length, nullptr, 0, nullptr,
                                 nullptr);
  std::string out(size > 0 ? size : 0, '\0');
  if (size > 0) {
    WideCharToMultiByte(CP_UTF8, 0, text, length, out.data(), size, nullptr,
                        nullptr);
  }
  return out;
}

std::string GetStringAttribute(IMFAttributes* attributes, const GUID& key) {
  UINT32 length = 0;
  if (FAILED(attributes->GetStringLength(key, &length))) {
    return {};
  }
  std::wstring value(length + 1, L'\0');
  if (FAILED(attributes->GetString(key, value.data(), length + 1, nullptr))) {
    return {};
  }
  return ToUtf8(value.c_str(), static_cast<int>(length));
}

std::optional<LUID> GetAdapterLuid(IMFAttributes* attributes) {
  UINT64 packed = 0;
  if (SUCCEEDED(attributes->GetUINT64(MFT_ENUM_ADAPTER_LUID, &packed))) {
    LUID luid;
    luid.LowPart = static_cast<DWORD>(packed & 0xffffffff);
    luid.HighPart = static_cast<LONG>(packed >> 32);
    return luid;
  }
  LUID luid;
  UINT32 size = 0;
  if (SUCCEEDED(attributes->GetBlob(MFT_ENUM_ADAPTER_LUID,
                                    reinterpret_cast<UINT8*>(&luid),
                                    sizeof(luid), &size)) &&
      size == sizeof(luid)) {
    return luid;
  }
  return std::nullopt;
}

std::string Lowercase(std::string text) {
  std::transform(text.begin(), text.end(), text.begin(), [](unsigned char c) {
    return static_cast<char>(std::tolower(c));
  });
  return text;
}

bool MatchesOverride(const MftCandidate& candidate, const std::string& wanted) {
  if (wanted == "nvidia") {
    return candidate.vendor_id == "VEN_10DE";
  }
  if (wanted == "amd") {
    return candidate.vendor_id == "VEN_1002";
  }
  if (wanted == "intel") {
    return candidate.vendor_id == "VEN_8086";
  }
  return Lowercase(candidate.name).find(wanted) != std::string::npos;
}

}  // namespace

bool EnsureComInitialized() {
  thread_local const bool initialized = [] {
    HRESULT hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    // RPC_E_CHANGED_MODE: the thread is already in an apartment, which is
    // just as good for the free-threaded objects used here.
    return SUCCEEDED(hr) || hr == RPC_E_CHANGED_MODE;
  }();
  return initialized;
}

bool EnsureMediaFoundationStarted() {
  static const bool started = [] {
    EnsureComInitialized();
    HRESULT hr = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
    if (FAILED(hr)) {
      RTC_LOG(LS_WARNING) << "MFStartup failed: " << HresultToString(hr);
      return false;
    }
    return true;
  }();
  return started;
}

std::vector<MftCandidate> EnumerateHardwareMfts(const GUID& category,
                                                const GUID& input_subtype,
                                                const GUID& output_subtype,
                                                const char* override_env) {
  std::vector<MftCandidate> candidates;
  std::string wanted;
  if (const char* value = std::getenv(override_env)) {
    wanted = Lowercase(value);
  }
  if (wanted == "off" || !EnsureComInitialized() ||
      !EnsureMediaFoundationStarted()) {
    return candidates;
  }

  MFT_REGISTER_TYPE_INFO input = {MFMediaType_Video, input_subtype};
  MFT_REGISTER_TYPE_INFO output = {MFMediaType_Video, output_subtype};
  const UINT32 flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
  auto take = [&](IMFActivate** activates, UINT32 count,
                  std::optional<LUID> luid) {
    for (UINT32 i = 0; i < count; ++i) {
      MftCandidate candidate;
      candidate.activate.Attach(activates[i]);
      candidate.name =
          GetStringAttribute(activates[i], MFT_FRIENDLY_NAME_Attribute);
      candidate.vendor_id = GetStringAttribute(
          activates[i], MFT_ENUM_HARDWARE_VENDOR_ID_Attribute);
      candidate.adapter_luid = luid ? luid : GetAdapterLuid(activates[i]);
      // Drivers register one transform for several adapters (virtual
      // displays among them); a second copy would only fail the same way.
      const bool repeated = std::any_of(
          candidates.begin(), candidates.end(),
          [&](const MftCandidate& c) { return c.name == candidate.name; });
      if (!repeated && (wanted.empty() || MatchesOverride(candidate, wanted))) {
        candidates.push_back(std::move(candidate));
      }
    }
    CoTaskMemFree(activates);
  };

  // Adapter by adapter, so that each transform comes with the adapter it runs
  // on, which a transform wanting a D3D11 device needs a device from. MFTEnum2
  // is looked up rather than linked: Windows 10 before 1703 lacks it.
  using MFTEnum2Fn = HRESULT(WINAPI*)(
      GUID, UINT32, const MFT_REGISTER_TYPE_INFO*,
      const MFT_REGISTER_TYPE_INFO*, IMFAttributes*, IMFActivate***, UINT32*);
  static const auto enum2 = reinterpret_cast<MFTEnum2Fn>(
      GetProcAddress(GetModuleHandleW(L"mfplat.dll"), "MFTEnum2"));
  for (const LUID& luid : enum2 ? HardwareAdapters() : std::vector<LUID>()) {
    ComPtr<IMFAttributes> attributes;
    IMFActivate** activates = nullptr;
    UINT32 count = 0;
    if (SUCCEEDED(MFCreateAttributes(&attributes, 1)) &&
        SUCCEEDED(attributes->SetBlob(MFT_ENUM_ADAPTER_LUID,
                                      reinterpret_cast<const UINT8*>(&luid),
                                      sizeof(luid))) &&
        SUCCEEDED(enum2(category, flags, &input, &output, attributes.Get(),
                        &activates, &count))) {
      take(activates, count, luid);
    }
  }
  if (candidates.empty()) {
    IMFActivate** activates = nullptr;
    UINT32 count = 0;
    HRESULT hr =
        MFTEnumEx(category, flags, &input, &output, &activates, &count);
    if (FAILED(hr)) {
      RTC_LOG(LS_WARNING) << "MFTEnumEx failed: " << HresultToString(hr);
      return candidates;
    }
    take(activates, count, std::nullopt);
  }

  if (!wanted.empty() && candidates.empty()) {
    RTC_LOG(LS_WARNING) << override_env << "=" << wanted
                        << " matches no hardware Media Foundation transform";
  }
  return candidates;
}

std::vector<LUID> HardwareAdapters() {
  ComPtr<IDXGIFactory1> factory;
  if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&factory)))) {
    return {};
  }
  std::vector<std::pair<SIZE_T, LUID>> adapters;
  ComPtr<IDXGIAdapter1> adapter;
  for (UINT i = 0; factory->EnumAdapters1(i, &adapter) != DXGI_ERROR_NOT_FOUND;
       ++i) {
    DXGI_ADAPTER_DESC1 desc;
    if (SUCCEEDED(adapter->GetDesc1(&desc)) &&
        !(desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE)) {
      adapters.emplace_back(desc.DedicatedVideoMemory, desc.AdapterLuid);
    }
    adapter.Reset();
  }
  std::stable_sort(
      adapters.begin(), adapters.end(),
      [](const auto& a, const auto& b) { return a.first > b.first; });
  std::vector<LUID> luids;
  for (const auto& entry : adapters) {
    luids.push_back(entry.second);
  }
  return luids;
}

std::optional<D3DDevice> CreateD3DDevice(std::optional<LUID> luid) {
  ComPtr<IDXGIAdapter1> adapter;
  if (luid) {
    ComPtr<IDXGIFactory1> factory;
    if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&factory)))) {
      return std::nullopt;
    }
    ComPtr<IDXGIAdapter1> candidate;
    for (UINT i = 0;
         factory->EnumAdapters1(i, &candidate) != DXGI_ERROR_NOT_FOUND; ++i) {
      DXGI_ADAPTER_DESC1 desc;
      if (SUCCEEDED(candidate->GetDesc1(&desc)) &&
          desc.AdapterLuid.LowPart == luid->LowPart &&
          desc.AdapterLuid.HighPart == luid->HighPart) {
        adapter = candidate;
        break;
      }
      candidate.Reset();
    }
    if (!adapter) {
      RTC_LOG(LS_WARNING) << "No DXGI adapter matches the transform's LUID";
      return std::nullopt;
    }
  }

  static const D3D_FEATURE_LEVEL kLevels[] = {
      D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1,
      D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_9_3};
  D3DDevice d3d;
  HRESULT hr = D3D11CreateDevice(
      adapter.Get(),
      adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE, nullptr,
      D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
      kLevels, ARRAYSIZE(kLevels), D3D11_SDK_VERSION, &d3d.device, nullptr,
      nullptr);
  if (FAILED(hr)) {
    RTC_LOG(LS_WARNING) << "D3D11CreateDevice failed: " << HresultToString(hr);
    return std::nullopt;
  }
  // The transform uses the device from its own threads.
  ComPtr<ID3D10Multithread> multithread;
  if (SUCCEEDED(d3d.device.As(&multithread))) {
    multithread->SetMultithreadProtected(TRUE);
  }
  hr = MFCreateDXGIDeviceManager(&d3d.reset_token, &d3d.manager);
  if (SUCCEEDED(hr)) {
    hr = d3d.manager->ResetDevice(d3d.device.Get(), d3d.reset_token);
  }
  if (FAILED(hr)) {
    RTC_LOG(LS_WARNING) << "DXGI device manager failed: "
                        << HresultToString(hr);
    return std::nullopt;
  }
  return d3d;
}

std::string HresultToString(HRESULT hr) {
  char text[16];
  std::snprintf(text, sizeof(text), "0x%08lX", static_cast<unsigned long>(hr));
  return text;
}

}  // namespace mf
}  // namespace webrtc
