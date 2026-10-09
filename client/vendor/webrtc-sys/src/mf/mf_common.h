#ifndef WEBRTC_MF_COMMON_H_
#define WEBRTC_MF_COMMON_H_

#include <d3d11.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mftransform.h>
#include <windows.h>
#include <wrl/client.h>

#include <cstdint>
#include <optional>
#include <string>
#include <vector>

namespace webrtc {
namespace mf {

using Microsoft::WRL::ComPtr;

// Picks or disables the hardware H.264 encoder; see EnumerateHardwareMfts.
constexpr char kEncoderOverrideEnv[] = "LIVEKIT_MF_H264_ENCODER";

// Media Foundation objects are COM objects; WebRTC's codec threads never
// initialize COM themselves. Joins the calling thread to the multithreaded
// apartment once and leaves it there for the thread's lifetime.
bool EnsureComInitialized();

// MFStartup once per process. Never paired with MFShutdown: other parts of
// the process (camera capture) use Media Foundation too.
bool EnsureMediaFoundationStarted();

struct MftCandidate {
  ComPtr<IMFActivate> activate;
  std::string name;
  // "VEN_10DE" for NVIDIA, "VEN_1002" for AMD, "VEN_8086" for Intel.
  std::string vendor_id;
  std::optional<LUID> adapter_luid;
};

// Hardware MFTs of `category` converting `input_subtype` to `output_subtype`,
// in Media Foundation's preference order, filtered by the environment
// variable `override_env` when set: "off" for none, "nvidia", "amd" or
// "intel" for a vendor, anything else matched against the friendly name.
std::vector<MftCandidate> EnumerateHardwareMfts(const GUID& category,
                                                const GUID& input_subtype,
                                                const GUID& output_subtype,
                                                const char* override_env);

// Hardware adapters, those with the most dedicated memory (discrete cards)
// first: on a hybrid machine that is the card a game being shared runs on.
std::vector<LUID> HardwareAdapters();

// A video-capable D3D11 device on the adapter with `luid` (the default adapter
// when absent), safe to use from several threads, and a Media Foundation
// device manager around it for MFT_MESSAGE_SET_D3D_MANAGER.
struct D3DDevice {
  ComPtr<ID3D11Device> device;
  ComPtr<IMFDXGIDeviceManager> manager;
  UINT reset_token = 0;
};
std::optional<D3DDevice> CreateD3DDevice(std::optional<LUID> luid);

std::string HresultToString(HRESULT hr);

}  // namespace mf
}  // namespace webrtc

#endif  // WEBRTC_MF_COMMON_H_
