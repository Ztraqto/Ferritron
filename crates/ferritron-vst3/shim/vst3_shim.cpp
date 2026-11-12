// VST3 format shim of Ferritron.
//
// Compiled against the VST3 SDK that the plugin developer provides
// (VST3_SDK_DIR). It implements the VST3 interfaces on top of the
// format-independent C ABI in ferritron_abi.h; it contains no DSP and no
// plugin-specific code.
//
// One object implements IComponent, IAudioProcessor and IEditController
// together (the "single component" model), so processor and controller
// share their parameters directly.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

#include "ferritron_abi.h"
#include "ferritron_events.h"

#include "pluginterfaces/base/ibstream.h"
#include "pluginterfaces/base/iplugincompatibility.h"
#include "pluginterfaces/base/ipluginbase.h"
#include "pluginterfaces/gui/iplugview.h"
#include "pluginterfaces/gui/iplugviewcontentscalesupport.h"
#include "pluginterfaces/vst/ivstattributes.h"
#include "pluginterfaces/vst/ivstaudioprocessor.h"
#include "pluginterfaces/vst/ivstchannelcontextinfo.h"
#include "pluginterfaces/vst/ivstcomponent.h"
#include "pluginterfaces/vst/ivsteditcontroller.h"
#include "pluginterfaces/vst/ivstevents.h"
#include "pluginterfaces/vst/ivstmessage.h"
#include "pluginterfaces/vst/ivstnoteexpression.h"
#include "pluginterfaces/vst/ivstparameterchanges.h"
#include "pluginterfaces/vst/ivstremapparamid.h"
#include "pluginterfaces/vst/ivstunits.h"
#include "pluginterfaces/vst/vstspeaker.h"
#ifdef FERRITRON_HAVE_ARA
#include "ARA_API/ARAVST3.h"
#endif

#if SMTG_OS_WINDOWS
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#elif SMTG_OS_MACOS
#include <dispatch/dispatch.h>
#endif

#include <algorithm>
#include <mutex>
#include <cmath>
#include <cstdio>
#include <atomic>
#include <cstring>
#include <string>
#include <utility>
#include <vector>

using namespace Steinberg;
using namespace Steinberg::Vst;

namespace {

constexpr int32 kChannels = 32; // most channels per bus
// Hidden parameters that carry MIDI controllers to the plugin (IMidiMapping): per channel,
// controllers 0..127, 128 = channel pressure, 129 = pitch bend, 130 = program change.
constexpr ParamID kMidiParamBase = 0x40001000u;
constexpr int32 kMidiCtrls = 131;
constexpr int32 kMidiParamCount = 16 * kMidiCtrls;
constexpr bool isMidiParam(ParamID id) { return id >= kMidiParamBase && id < kMidiParamBase + 16 * 256; }

// A VST3 speaker bit as a Ferritron speaker id (FERRITRON_SPEAKER_*).
uint32_t speakerId(int bit) {
	switch (bit) {
	case 0: return FERRITRON_SPEAKER_L;
	case 1: return FERRITRON_SPEAKER_R;
	case 2: return FERRITRON_SPEAKER_C;
	case 3: return FERRITRON_SPEAKER_LFE;
	case 4: return FERRITRON_SPEAKER_LS;
	case 5: return FERRITRON_SPEAKER_RS;
	case 6: return FERRITRON_SPEAKER_LC;
	case 7: return FERRITRON_SPEAKER_RC;
	case 8: return FERRITRON_SPEAKER_CS;
	case 9: return FERRITRON_SPEAKER_LSS;
	case 10: return FERRITRON_SPEAKER_RSS;
	case 11: return FERRITRON_SPEAKER_TC;
	case 12: return FERRITRON_SPEAKER_TFL;
	case 13: return FERRITRON_SPEAKER_TFC;
	case 14: return FERRITRON_SPEAKER_TFR;
	case 15: return FERRITRON_SPEAKER_TRL;
	case 16: return FERRITRON_SPEAKER_TRC;
	case 17: return FERRITRON_SPEAKER_TRR;
	case 18: return FERRITRON_SPEAKER_LFE2;
	case 19: return FERRITRON_SPEAKER_C; // mono
	case 24: return FERRITRON_SPEAKER_TSL;
	case 25: return FERRITRON_SPEAKER_TSR;
	case 28: return FERRITRON_SPEAKER_BFL;
	case 29: return FERRITRON_SPEAKER_BFC;
	case 30: return FERRITRON_SPEAKER_BFR;
	case 35: return FERRITRON_SPEAKER_BRL;
	default: return FERRITRON_SPEAKER_OTHER + static_cast<uint32_t>(bit);
	}
}
constexpr uint32_t kMaxEvents = 2048; // automation points and notes per block; more are applied at once, untimed

bool sameIid(const void* a, const TUID b) {
	return a && std::memcmp(a, b, sizeof(TUID)) == 0;
}

// --- Strings -----------------------------------------------------------------

// Copies UTF-8 bytes into a null-terminated char8 buffer without splitting a character.
void copyUtf8(char8* dst, size_t cap, ferritron_str s) {
	size_t len = std::min<size_t>(s.ptr ? s.len : 0, cap - 1);
	while (len > 0 && len < s.len && (static_cast<unsigned char>(s.ptr[len]) & 0xC0) == 0x80)
		--len;
	if (len)
		std::memcpy(dst, s.ptr, len);
	dst[len] = 0;
}

// Decodes UTF-8 into a null-terminated UTF-16 buffer, truncating if needed.
void copyUtf16(char16* dst, size_t cap, const char* src, size_t srcLen) {
	size_t out = 0;
	size_t i = 0;
	while (i < srcLen) {
		unsigned char c = static_cast<unsigned char>(src[i]);
		uint32 cp;
		size_t extra;
		if (c < 0x80) { cp = c; extra = 0; }
		else if ((c & 0xE0) == 0xC0) { cp = c & 0x1F; extra = 1; }
		else if ((c & 0xF0) == 0xE0) { cp = c & 0x0F; extra = 2; }
		else { cp = c & 0x07; extra = 3; }
		if (extra && i + extra >= srcLen)
			break; // truncated sequence
		for (size_t k = 1; k <= extra; ++k)
			cp = (cp << 6) | (static_cast<unsigned char>(src[i + k]) & 0x3F);
		i += extra + 1;
		size_t units = cp >= 0x10000 ? 2 : 1;
		if (out + units >= cap)
			break;
		if (units == 2) {
			cp -= 0x10000;
			dst[out++] = static_cast<char16>(0xD800 + (cp >> 10));
			dst[out++] = static_cast<char16>(0xDC00 + (cp & 0x3FF));
		} else {
			dst[out++] = static_cast<char16>(cp);
		}
	}
	dst[out] = 0;
}

void copyUtf16(char16* dst, size_t cap, ferritron_str s) {
	copyUtf16(dst, cap, s.ptr ? s.ptr : "", s.ptr ? s.len : 0);
}

// Encodes a null-terminated UTF-16 string as UTF-8.
std::string toUtf8(const char16* src) {
	std::string out;
	for (size_t i = 0; src && src[i] && i < 1024; ++i) {
		uint32 cp = src[i];
		if (cp >= 0xD800 && cp < 0xDC00 && src[i + 1] >= 0xDC00 && src[i + 1] < 0xE000)
			cp = 0x10000 + ((cp - 0xD800) << 10) + (src[++i] - 0xDC00);
		if (cp < 0x80) {
			out += static_cast<char>(cp);
		} else if (cp < 0x800) {
			out += static_cast<char>(0xC0 | (cp >> 6));
			out += static_cast<char>(0x80 | (cp & 0x3F));
		} else if (cp < 0x10000) {
			out += static_cast<char>(0xE0 | (cp >> 12));
			out += static_cast<char>(0x80 | ((cp >> 6) & 0x3F));
			out += static_cast<char>(0x80 | (cp & 0x3F));
		} else {
			out += static_cast<char>(0xF0 | (cp >> 18));
			out += static_cast<char>(0x80 | ((cp >> 12) & 0x3F));
			out += static_cast<char>(0x80 | ((cp >> 6) & 0x3F));
			out += static_cast<char>(0x80 | (cp & 0x3F));
		}
	}
	return out;
}

// --- Streams -----------------------------------------------------------------

bool readAll(IBStream* stream, std::vector<uint8_t>& out) {
	if (!stream)
		return false;
	uint8_t chunk[4096];
	for (;;) {
		int32 read = 0;
		if (stream->read(chunk, sizeof(chunk), &read) != kResultOk || read <= 0)
			return true;
		out.insert(out.end(), chunk, chunk + read);
	}
}

bool writeAll(IBStream* stream, const std::vector<uint8_t>& data) {
	if (!stream)
		return false;
	size_t done = 0;
	while (done < data.size()) {
		int32 written = 0;
		auto rest = static_cast<int32>(data.size() - done);
		if (stream->write(const_cast<uint8_t*>(data.data() + done), rest, &written) != kResultOk || written <= 0)
			return false;
		done += static_cast<size_t>(written);
	}
	return true;
}


// --- VST3 options the macro generates (`ferritron_vst3_options`) --------------
//
// Layout (see `vst3_options` in ferritron-macros): version 1, flags (1 controller class,
// 2 compatibility class), controller ID, compatibility class ID, replaced class IDs, and
// (old, new) pairs of parameter IDs for IRemapParamID. A plugin without the symbol has none.
extern "C" const uint8_t ferritron_vst3_options[];
extern "C" const uint8_t ferritron_vst3_no_options[] = {0};
#if defined(_MSC_VER)
#pragma comment(linker, "/alternatename:ferritron_vst3_options=ferritron_vst3_no_options")
#endif

struct Vst3Options {
	bool controller = false;
	bool compat = false;
	uint8_t controllerId[16] = {};
	uint8_t compatId[16] = {};
	std::vector<std::vector<uint8_t>> replaces; // the digits in the order written
	std::vector<std::pair<uint32_t, uint32_t>> remap;
};

const Vst3Options& options() {
	static const Vst3Options parsed = [] {
		Vst3Options o;
		const uint8_t* b = ferritron_vst3_options;
		if (!b || b[0] != 1)
			return o;
		o.controller = (b[1] & 1) != 0;
		o.compat = (b[1] & 2) != 0;
		std::memcpy(o.controllerId, b + 2, 16);
		std::memcpy(o.compatId, b + 18, 16);
		size_t at = 34;
		for (int n = b[at++]; n > 0; --n, at += 16)
			o.replaces.emplace_back(b + at, b + at + 16);
		for (int pairs = b[at++]; pairs > 0; --pairs, at += 8) {
			uint32_t oldId, newId;
			std::memcpy(&oldId, b + at, 4);
			std::memcpy(&newId, b + at + 4, 4);
			o.remap.emplace_back(oldId, newId);
		}
		return o;
	}();
	return parsed;
}

// The 32 hex digits a host shows for 16 bytes of class ID (FUID::toString).
std::string uidString(const uint8_t* id) {
	uint8_t b[16];
	std::memcpy(b, id, 16);
#if COM_COMPATIBLE
	std::reverse(b, b + 4);
	std::reverse(b + 4, b + 6);
	std::reverse(b + 6, b + 8);
#endif
	static const char* hex = "0123456789ABCDEF";
	std::string out;
	for (uint8_t x : b) {
		out += hex[x >> 4];
		out += hex[x & 15];
	}
	return out;
}

// 16 bytes as plain hex digits, the way the replaced plug-in's own moduleinfo.json wrote them.
std::string plainHex(const uint8_t* id) {
	static const char* hex = "0123456789ABCDEF";
	std::string out;
	for (int i = 0; i < 16; ++i) {
		out += hex[id[i] >> 4];
		out += hex[id[i] & 15];
	}
	return out;
}

// --- Plugin instance ---------------------------------------------------------

// IComponent and IEditController both declare setState/getState with the same
// signature but different meanings; these bases give each its own name.
class ComponentBase : public IComponent {
public:
	tresult PLUGIN_API setState(IBStream* state) final { return setComponentStateImpl(state); }
	tresult PLUGIN_API getState(IBStream* state) final { return getComponentStateImpl(state); }

protected:
	virtual tresult setComponentStateImpl(IBStream* state) = 0;
	virtual tresult getComponentStateImpl(IBStream* state) = 0;
};

class ControllerBase : public IEditController {
public:
	tresult PLUGIN_API setState(IBStream* state) final { return setControllerStateImpl(state); }
	tresult PLUGIN_API getState(IBStream* state) final { return getControllerStateImpl(state); }

protected:
	virtual tresult setControllerStateImpl(IBStream* state) = 0;
	virtual tresult getControllerStateImpl(IBStream* state) = 0;
};

class Plugin;

// Only Ferritron objects answer this: how the two halves of a `[vst3] controller_id` plugin find each other.
static const TUID kPeerIid = INLINE_UID(0x67A0600B, 0x7D414892, 0xBFE127FA, 0x577935D1);

/* Edits the plugin makes by itself (e.g. an AI client through the Ferritron
   MCP hub) arrive on any thread. They are queued and handed to the host on
   the main thread: on Windows by a timer that lives while plugins do, on
   macOS through the main dispatch queue. */
namespace host_edits {
// FERRITRON_EDIT_BEGIN / PERFORM / END, or ONE_SHOT for all three (ferritron.host.v1).
constexpr uint32_t ONE_SHOT = 0xFFFFFFFFu;
struct Edit {
	Plugin* plugin;
	uint32_t kind;
	uint32_t id;
	double normalized;
};
std::mutex lock;
std::vector<Edit> queue;
std::vector<Plugin*> registry; // live plugins, for the main-thread poll (guarded by `lock`)
int plugins = 0;
#ifdef _WIN32
UINT_PTR timer = 0;
void CALLBACK tick(HWND, UINT, UINT_PTR, DWORD);
#endif
#if defined(_WIN32) || defined(__APPLE__)
void drain(void*);
#endif
void deliver(Plugin* plugin, uint32_t kind, uint32_t id, double normalized);
void push(Plugin* plugin, uint32_t kind, uint32_t id, double normalized);
} // namespace host_edits

class Editor;

// ARA 2 (ferritron.ara.v1), when the plugin has it and the shim was built with the ARA SDK.
const ferritron_ara_api* araApi(const ferritron_plugin_api* api) {
#ifdef FERRITRON_HAVE_ARA
	return static_cast<const ferritron_ara_api*>(api->get_extension(FERRITRON_EXT_ARA));
#else
	(void)api;
	return nullptr;
#endif
}

class Plugin final : public ComponentBase,
                     public IAudioProcessor,
                     public ControllerBase,
                     public IProcessContextRequirements,
                     public IMidiMapping,
                     public IRemapParamID,
                     public IUnitInfo,
                     public INoteExpressionController,
#ifdef FERRITRON_HAVE_ARA
                     public ARA::IPlugInEntryPoint,
                     public ARA::IPlugInEntryPoint2,
#endif
                     public ChannelContext::IInfoListener,
                     public IConnectionPoint {
public:
	Plugin(const ferritron_plugin_api* api, void* core)
	: api(api), core(core), instrument(api->info->kind == FERRITRON_PLUGIN_INSTRUMENT) {
		link = static_cast<const ferritron_host_api*>(api->get_extension(FERRITRON_EXT_HOST));
		track = static_cast<const ferritron_track_api*>(api->get_extension(FERRITRON_EXT_TRACK));
		transportApi = static_cast<const ferritron_transport_api*>(api->get_extension(FERRITRON_EXT_TRANSPORT));
		mainApi = static_cast<const ferritron_main_api*>(api->get_extension(FERRITRON_EXT_MAIN));
		findProgramParam();
		if (const auto* v = static_cast<const ferritron_voices_api*>(api->get_extension(FERRITRON_EXT_VOICES)))
			voices = v->capacity(core);
		sidechainApi = static_cast<const ferritron_sidechain_api*>(api->get_extension(FERRITRON_EXT_SIDECHAIN));
		layoutApi = static_cast<const ferritron_layout_api*>(api->get_extension(FERRITRON_EXT_LAYOUT));
		const ferritron_plugin_info& pi = *api->info;
		hasSidechain = (pi.audio_flags & FERRITRON_AUDIO_SIDECHAIN) != 0 && sidechainApi && !instrument;
		midiIn = (pi.audio_flags & FERRITRON_AUDIO_MIDI_INPUT) != 0;
		midiMap = midiIn && (pi.audio_flags & FERRITRON_AUDIO_HOST_CC) == 0;
		minCh = static_cast<int32>(pi.min_channels);
		maxCh = static_cast<int32>(pi.max_channels);
		if (maxCh < minCh)
			maxCh = minCh;
		passthrough = (pi.audio_flags & FERRITRON_AUDIO_PASSTHROUGH) != 0;
		scFixed = static_cast<int32>(pi.sidechain_channels);
		// Stereo when the plugin allows it, else the narrowest it accepts.
		busChannels = std::clamp<int32>(2, minCh, maxCh);
		mainArr = arrangementOf(busChannels);
		scChannels = scFixed > 0 ? scFixed : busChannels;
		scArr = arrangementOf(scChannels);
		applyLayout();
		// Edits the plugin makes by itself; ctx is `this`, as for the editor's view host.
		if (auto* link2 = static_cast<const ferritron_host_api2*>(api->get_extension(FERRITRON_EXT_HOST2))) {
			ferritron_host_callbacks2 callbacks{
			    this,
			    [](void* ctx, uint32_t kind, uint32_t id, double value) { host_edits::push(static_cast<Plugin*>(ctx), kind, id, value); },
			    nullptr, // VST3 hosts keep calling process
			};
			link2->attach(core, &callbacks);
		} else if (link) {
			ferritron_host_callbacks callbacks{this, [](void* ctx, uint32_t id, double value) {
				                                   host_edits::push(static_cast<Plugin*>(ctx), host_edits::ONE_SHOT, id, value);
			                                   }};
			link->attach(core, &callbacks);
		}
		std::lock_guard<std::mutex> guard(host_edits::lock);
		host_edits::registry.push_back(this);
#ifdef _WIN32
		if (host_edits::plugins++ == 0) // created on the host's main thread
			host_edits::timer = SetTimer(nullptr, 0, 30, host_edits::tick);
#else
		++host_edits::plugins;
#endif
	}

	~Plugin() {
		unpeer();
		{
			std::lock_guard<std::mutex> guard(host_edits::lock);
			auto& q = host_edits::queue;
			q.erase(std::remove_if(q.begin(), q.end(), [this](const host_edits::Edit& e) { return e.plugin == this; }), q.end());
			auto& reg = host_edits::registry;
			reg.erase(std::remove(reg.begin(), reg.end(), this), reg.end());
#ifdef _WIN32
			if (--host_edits::plugins == 0 && host_edits::timer) {
				KillTimer(nullptr, host_edits::timer);
				host_edits::timer = 0;
			}
#else
			--host_edits::plugins;
#endif
		}
		setComponentHandler(nullptr);
		api->destroy(core);
	}

	// ChannelContext::IInfoListener: the track the plugin sits on.
	tresult PLUGIN_API setChannelContextInfos(IAttributeList* list) override {
		if (separateController && peer) // a connected controller: the processor's instance is the one that has the track
			return peer->setChannelContextInfos(list);
		if (!list || (!link && !track))
			return kResultOk;
		String128 name{};
		bool hasName = list->getString(ChannelContext::kChannelNameKey, name, sizeof(name)) == kResultOk;
		char utf8[512];
		int len = 0;
		for (int i = 0; name[i] && len < static_cast<int>(sizeof(utf8)) - 4; ++i) {
			uint32_t c = static_cast<uint16_t>(name[i]);
			if (c >= 0xD800 && c < 0xDC00 && name[i + 1]) // surrogate pair
				c = 0x10000 + ((c - 0xD800) << 10) + (static_cast<uint16_t>(name[++i]) - 0xDC00);
			if (c < 0x80) {
				utf8[len++] = static_cast<char>(c);
			} else if (c < 0x800) {
				utf8[len++] = static_cast<char>(0xC0 | (c >> 6));
				utf8[len++] = static_cast<char>(0x80 | (c & 0x3F));
			} else if (c < 0x10000) {
				utf8[len++] = static_cast<char>(0xE0 | (c >> 12));
				utf8[len++] = static_cast<char>(0x80 | ((c >> 6) & 0x3F));
				utf8[len++] = static_cast<char>(0x80 | (c & 0x3F));
			} else {
				utf8[len++] = static_cast<char>(0xF0 | (c >> 18));
				utf8[len++] = static_cast<char>(0x80 | ((c >> 12) & 0x3F));
				utf8[len++] = static_cast<char>(0x80 | ((c >> 6) & 0x3F));
				utf8[len++] = static_cast<char>(0x80 | (c & 0x3F));
			}
		}
		if (!hasName)
			len = 0;
		if (track) {
			ferritron_track_info info{};
			info.name = ferritron_str{utf8, static_cast<uint32_t>(len)};
			if (hasName)
				info.flags |= FERRITRON_TRACK_HAS_NAME;
			int64 color = 0;
			if (list->getInt(ChannelContext::kChannelColorKey, color) == kResultOk) {
				auto spec = static_cast<ChannelContext::ColorSpec>(color);
				info.color[0] = ChannelContext::GetRed(spec);
				info.color[1] = ChannelContext::GetGreen(spec);
				info.color[2] = ChannelContext::GetBlue(spec);
				info.flags |= FERRITRON_TRACK_HAS_COLOR;
			}
			int64 index = 0;
			if (list->getInt(ChannelContext::kChannelIndexKey, index) == kResultOk && index >= 1) {
				info.index = static_cast<uint32_t>(index - 1); // VST3 counts from 1
				info.flags |= FERRITRON_TRACK_HAS_INDEX;
			}
			int64 location = 0;
			if (list->getInt(ChannelContext::kChannelPluginLocationKey, location) == kResultOk && location >= 0 && location <= 2) {
				info.location = static_cast<uint32_t>(location);
				info.flags |= FERRITRON_TRACK_HAS_LOCATION;
			}
			track->set_track_info(core, &info);
		} else if (hasName) {
			link->set_track_name(core, ferritron_str{utf8, static_cast<uint32_t>(len)});
		}
		return kResultOk;
	}

	// An edit the plugin made by itself, on the main thread.
	void hostEdited(uint32_t kind, uint32_t id, double normalized) {
		IComponentHandler* h = hostHandler();
		if (!h)
			return;
		if (kind == host_edits::ONE_SHOT || kind == FERRITRON_EDIT_BEGIN)
			h->beginEdit(id);
		if (kind == host_edits::ONE_SHOT || kind == FERRITRON_EDIT_PERFORM)
			h->performEdit(id, normalized);
		if (kind == host_edits::ONE_SHOT || kind == FERRITRON_EDIT_END)
			h->endEdit(id);
	}

	// FUnknown
	tresult PLUGIN_API queryInterface(const TUID requested, void** obj) override {
		if (!obj)
			return kInvalidArgument;
		if (sameIid(requested, FUnknown_iid) || sameIid(requested, IPluginBase_iid) || sameIid(requested, IComponent_iid))
			*obj = static_cast<IComponent*>(this);
		else if (sameIid(requested, IAudioProcessor_iid))
			*obj = static_cast<IAudioProcessor*>(this);
		else if (sameIid(requested, IEditController_iid))
			*obj = static_cast<IEditController*>(this);
		else if (midiMap && sameIid(requested, IMidiMapping_iid))
			*obj = static_cast<IMidiMapping*>(this);
		else if (!options().remap.empty() && sameIid(requested, IRemapParamID_iid))
			*obj = static_cast<IRemapParamID*>(this);
		else if (sameIid(requested, IProcessContextRequirements_iid))
			*obj = static_cast<IProcessContextRequirements*>(this);
		else if (programCount > 0 && sameIid(requested, IUnitInfo_iid))
			*obj = static_cast<IUnitInfo*>(this);
		else if (voices > 0 && sameIid(requested, INoteExpressionController_iid))
			*obj = static_cast<INoteExpressionController*>(this);
		else if (sameIid(requested, ChannelContext::IInfoListener_iid))
			*obj = static_cast<ChannelContext::IInfoListener*>(this);
		else if (sameIid(requested, IConnectionPoint_iid))
			*obj = static_cast<IConnectionPoint*>(this);
		else if (sameIid(requested, kPeerIid))
			*obj = this;
#ifdef FERRITRON_HAVE_ARA
		else if (ara && sameIid(requested, ARA::IPlugInEntryPoint_iid))
			*obj = static_cast<ARA::IPlugInEntryPoint*>(this);
		else if (ara && sameIid(requested, ARA::IPlugInEntryPoint2_iid))
			*obj = static_cast<ARA::IPlugInEntryPoint2*>(this);
#endif
		else {
			*obj = nullptr;
			return kNoInterface;
		}
		addRef();
		return kResultOk;
	}

	uint32 PLUGIN_API addRef() override { return ++refs; }

	uint32 PLUGIN_API release() override {
		uint32 remaining = --refs;
		if (remaining == 0)
			delete this;
		return remaining;
	}

	// IPluginBase (shared by the component and controller sides)
	tresult PLUGIN_API initialize(FUnknown*) override { return kResultOk; }

	tresult PLUGIN_API terminate() override {
		setComponentHandler(nullptr);
		return kResultOk;
	}

	// IComponent
	tresult PLUGIN_API getControllerClassId(TUID classId) override {
		if (!options().controller)
			return kResultFalse; // no separate controller: query IEditController from this object
		std::memcpy(classId, options().controllerId, sizeof(TUID));
		return kResultOk;
	}

	// IRemapParamID: Ferritron 1.3.0 hashed the IDs of its built-in parameters (Bypass, Quality,
	// AI Access); from 1.4.0 they are fixed values from 0x40000000. A host replacing a plug-in of
	// this class (an older version of this one) asks where an automated parameter went.
	tresult PLUGIN_API getCompatibleParamID(const TUID pluginToReplaceUID, ParamID oldParamID, ParamID& newParamID) override {
		if (!pluginToReplaceUID || std::memcmp(pluginToReplaceUID, api->info->class_id, sizeof(TUID)) != 0)
			return kResultFalse; // the parameters of other plug-ins are not known here
		for (const auto& pair : options().remap) {
			if (pair.first == oldParamID) {
				newParamID = pair.second;
				return kResultTrue;
			}
		}
		// Every other parameter kept its ID.
		for (uint32_t i = 0, n = api->param_count(); i < n; ++i) {
			ferritron_param_info info{};
			if (api->param_info(i, &info) && info.id == oldParamID) {
				newParamID = oldParamID;
				return kResultTrue;
			}
		}
		return kResultFalse;
	}

	tresult PLUGIN_API setIoMode(IoMode) override { return kResultOk; }

	// Effects: one stereo audio input and output. Instruments: one note input
	// (event bus) and one stereo audio output.
	int32 PLUGIN_API getBusCount(MediaType type, BusDirection dir) override {
		if (type == kAudio) {
			if (dir == kOutput)
				return 1;
			return (instrument ? 0 : 1) + (hasSidechain ? 1 : 0);
		}
		if (type == kEvent)
			return dir == kInput && (instrument || midiIn) ? 1 : 0;
		return 0;
	}

	bool isBus(MediaType type, BusDirection dir, int32 index) {
		return index >= 0 && index < getBusCount(type, dir);
	}

	tresult PLUGIN_API getBusInfo(MediaType type, BusDirection dir, int32 index, BusInfo& bus) override {
		if (!isBus(type, dir, index))
			return kInvalidArgument;
		bus.mediaType = type;
		bus.direction = dir;
		const bool side = type == kAudio && dir == kInput && index == 1;
		bus.channelCount = type == kAudio ? (side ? scChannels : busChannels) : 16;
		const char* name = type == kEvent ? "Notes" : side ? "Sidechain" : dir == kInput ? "Input" : "Output";
		copyUtf16(bus.name, 128, name, std::strlen(name));
		bus.busType = side ? kAux : kMain;
		bus.flags = side ? 0 : BusInfo::kDefaultActive; // the sidechain starts inactive
		return kResultOk;
	}

	tresult PLUGIN_API getRoutingInfo(RoutingInfo&, RoutingInfo&) override { return kNotImplemented; }

	tresult PLUGIN_API activateBus(MediaType type, BusDirection dir, int32 index, TBool state) override {
		if (!isBus(type, dir, index))
			return kInvalidArgument;
		if (type == kAudio && dir == kInput && index == 1)
			scActive = state != 0;
		return kResultOk;
	}

	// The VST3 arrangement with `n` channels that hosts use (mono, stereo, 5.1, 7.1, ...), else the first n speakers.
	static SpeakerArrangement arrangementOf(int32 n) {
		switch (n) {
		case 1: return SpeakerArr::kMono;
		case 2: return SpeakerArr::kStereo;
		case 3: return SpeakerArr::k30Cine;
		case 4: return SpeakerArr::k40Music;
		case 5: return SpeakerArr::k50;
		case 6: return SpeakerArr::k51;
		case 8: return SpeakerArr::k71Cine;
		default: return n >= 64 ? ~SpeakerArrangement{0} : ((SpeakerArrangement{1} << n) - 1);
		}
	}

	// Tells the plugin which speaker each channel of the main bus is.
	void applyLayout() {
		if (!layoutApi)
			return;
		uint32_t ids[kChannels];
		uint32_t n = 0;
		for (int bit = 0; bit < 64 && n < kChannels; ++bit)
			if (mainArr & (SpeakerArrangement{1} << bit))
				ids[n++] = speakerId(bit);
		layoutApi->set(core, ids, n);
	}

	// IMidiMapping: controllers reach the plugin as hidden parameters.
	tresult PLUGIN_API getMidiControllerAssignment(int32 busIndex, int16 channel, CtrlNumber number, ParamID& id) override {
		if (!midiMap || busIndex != 0 || channel < 0 || channel > 15 || number < 0 || number >= kMidiCtrls)
			return kResultFalse;
		id = kMidiParamBase + static_cast<ParamID>(channel) * 256 + static_cast<ParamID>(number);
		return kResultTrue;
	}

#ifdef FERRITRON_HAVE_ARA
	// ARA::IPlugInEntryPoint / IPlugInEntryPoint2
	const ARA::ARAFactory* PLUGIN_API getFactory() override {
		return static_cast<const ARA::ARAFactory*>(ara->get_factory());
	}
	const ARA::ARAPlugInExtensionInstance* PLUGIN_API bindToDocumentController(ARA::ARADocumentControllerRef controller) override {
		// ARA 1 hosts: every role.
		constexpr uint32_t all = ARA::kARAPlaybackRendererRole | ARA::kARAEditorRendererRole | ARA::kARAEditorViewRole;
		return static_cast<const ARA::ARAPlugInExtensionInstance*>(ara->bind(core, controller, all, all));
	}
	const ARA::ARAPlugInExtensionInstance* PLUGIN_API bindToDocumentControllerWithRoles(
	    ARA::ARADocumentControllerRef controller, ARA::ARAPlugInInstanceRoleFlags known, ARA::ARAPlugInInstanceRoleFlags assigned) override {
		return static_cast<const ARA::ARAPlugInExtensionInstance*>(
		    ara->bind(core, controller, static_cast<uint32_t>(known), static_cast<uint32_t>(assigned)));
	}
#endif

	// Main thread, from the shim's timer: work the plugin asked of the host from other threads
	// (a new latency, parameter values changed in bulk).
	void pollMain() {
		// A latency change found in setActive is reported here, not from inside setActive.
		if (latencyPending) {
			latencyPending = false;
			if (IComponentHandler* h = hostHandler())
				h->restartComponent(kLatencyChanged);
		}
		if (!mainApi)
			return;
		uint32_t bits = mainApi->take_requests(core);
		if (bits & FERRITRON_REQUEST_LATENCY) {
			auto* ext = static_cast<const ferritron_latency_api*>(api->get_extension(FERRITRON_EXT_LATENCY));
			uint32 now = ext ? ext->get(core) : 0;
			if (now != latency) {
				latency = now;
				if (IComponentHandler* h = hostHandler())
					h->restartComponent(kLatencyChanged);
			}
		}
		IComponentHandler* h = hostHandler();
		if ((bits & FERRITRON_REQUEST_VALUES) && h) {
			h->restartComponent(kParamValuesChanged);
			// Not an automation edit, but the project has changed: mark it dirty.
			static const TUID kComponentHandler2Iid = INLINE_UID(0xF040B4B3, 0xA36045EC, 0xABCDC045, 0xB4D5A2CC);
			IComponentHandler2* handler2 = nullptr;
			if (h->queryInterface(kComponentHandler2Iid, reinterpret_cast<void**>(&handler2)) == kResultOk && handler2) {
				handler2->setDirty(true);
				handler2->release();
			}
		}
	}

	tresult PLUGIN_API setActive(TBool state) override {
		if (state)
			applyLayout();
		if (ara)
			ara->set_rendering(core, state != 0, sampleRate, static_cast<uint32_t>(busChannels), static_cast<uint32_t>(std::max<int32>(maxBlockSize, 0)));
		if (state) {
			if (renderExt)
				renderExt->set_offline(core, offline);
			liveOffline = offline;
			api->activate(core, sampleRate, static_cast<uint32_t>(std::max<int32>(maxBlockSize, 0)));
			auto* ext = static_cast<const ferritron_latency_api*>(api->get_extension(FERRITRON_EXT_LATENCY));
			uint32 now = ext ? ext->get(core) : 0;
			if (now != latency) {
				latency = now;
#ifdef _WIN32
				// Some hosts do not like restartComponent inside setActive: the main-thread timer
				// reports it on its next poll (pollMain). Other platforms have no such poll yet.
				latencyPending = true;
#else
				if (handler)
					handler->restartComponent(kLatencyChanged);
#endif
			}
		}
		return kResultOk; // a failed plugin outputs silence instead of failing the host
	}

	tresult setComponentStateImpl(IBStream* stream) override {
		std::vector<uint8_t> data;
		if (!readAll(stream, data))
			return kInvalidArgument;
		if (!api->load_state(core, data.data(), static_cast<uint32_t>(data.size())))
			return kResultFalse;
		restoreEditorSize();
		return kResultOk;
	}

	// The editor size saved with the state (ferritron.editor-size.v1).
	void restoreEditorSize();
	void saveEditorSize() {
		if (auto* ext = static_cast<const ferritron_editor_size_api*>(api->get_extension(FERRITRON_EXT_EDITOR_SIZE)))
			ext->set(core, editorWidth, editorHeight);
	}
	bool savedEditorSize(uint32_t& width, uint32_t& height) const {
		auto* ext = static_cast<const ferritron_editor_size_api*>(api->get_extension(FERRITRON_EXT_EDITOR_SIZE));
		return ext && ext->get(core, &width, &height);
	}
	Editor* openEditor = nullptr;

	tresult getComponentStateImpl(IBStream* stream) override {
		// The size can grow between the size query and the copy: try again, never save zeros.
		for (int attempt = 0; attempt < 3; ++attempt) {
			const uint32_t size = api->save_state(core, nullptr, 0);
			std::vector<uint8_t> data(size);
			const uint32_t written = api->save_state(core, data.data(), size);
			if (written <= size) {
				data.resize(written);
				return writeAll(stream, data) ? kResultOk : kResultFalse;
			}
		}
		return kResultFalse;
	}

	// IAudioProcessor
	tresult PLUGIN_API setBusArrangements(SpeakerArrangement* inputs, int32 numIns, SpeakerArrangement* outputs,
	                                      int32 numOuts) override {
		// Any arrangement whose channel count the plugin accepts; effects keep the input's layout.
		auto supported = [&](SpeakerArrangement a) {
			int32 n = SpeakerArr::getChannelCount(a);
			return passthrough ? (n >= 1 && n <= kChannels) : (n >= minCh && n <= maxCh);
		};
		const int32 wantIns = instrument ? 0 : 1;
		bool out = numOuts == 1 && outputs && supported(outputs[0]);
		bool in = numIns >= wantIns && numIns <= wantIns + (hasSidechain ? 1 : 0) && (numIns == 0 || inputs) && out &&
		          (instrument || inputs[0] == outputs[0]);
		if (!(in && out))
			return kResultFalse;
		if (hasSidechain && numIns == wantIns + 1) {
			int32 sc = SpeakerArr::getChannelCount(inputs[wantIns]);
			if (sc < 1 || sc > kChannels || (scFixed > 0 && sc != scFixed))
				return kResultFalse;
			scArr = inputs[wantIns];
			scChannels = sc;
		}
		mainArr = outputs[0];
		busChannels = SpeakerArr::getChannelCount(mainArr);
		passing = busChannels < minCh || busChannels > maxCh;
		if (hasSidechain && scFixed == 0 && numIns != wantIns + 1) {
			scArr = mainArr;
			scChannels = busChannels;
		}
		if (!passing)
			applyLayout();
		return kResultTrue;
	}

	tresult PLUGIN_API getBusArrangement(BusDirection dir, int32 index, SpeakerArrangement& arr) override {
		if (!isBus(kAudio, dir, index))
			return kInvalidArgument;
		arr = dir == kInput && index == 1 ? scArr : mainArr;
		return kResultOk;
	}

	tresult PLUGIN_API canProcessSampleSize(int32 size) override {
		return size == kSample32 ? kResultTrue : kResultFalse;
	}

	uint32 PLUGIN_API getLatencySamples() override { return latency; }

	tresult PLUGIN_API setupProcessing(ProcessSetup& setup) override {
		if (setup.symbolicSampleSize != kSample32)
			return kResultFalse;
		sampleRate = setup.sampleRate;
		maxBlockSize = setup.maxSamplesPerBlock;
		offline = setup.processMode == kOffline;
		return kResultOk;
	}

	tresult PLUGIN_API setProcessing(TBool) override { return kResultOk; }

	tresult PLUGIN_API process(ProcessData& data) override {
		// Gather automation points and notes, sorted by sample offset, without allocating.
		ferritron_event events[kMaxEvents];
		uint32_t numEvents = 0;
		// Parameter changes that did not fit in `events`: applied after the block, in order.
		constexpr uint32_t kMaxLate = 256;
		ferritron_event late[kMaxLate];
		uint32_t numLate = 0;
		// Collected in the order the host gives them, sorted once below (O(n log n)).
		auto insert = [&](const ferritron_event& ev) { events[numEvents++] = ev; };
		if (IEventList* list = (instrument || midiIn) ? data.inputEvents : nullptr) {
			for (int32 i = 0, n = list->getEventCount(); i < n && numEvents < kMaxEvents; ++i) {
				Event e{};
				if (list->getEvent(i, e) != kResultOk)
					continue;
				ferritron_event ev{};
				ev.offset = static_cast<uint32_t>(std::max<int32>(e.sampleOffset, 0));
				if (e.type == Event::kNoteOnEvent) {
					// Velocity 0 means note off, as in MIDI.
					ev.type = e.noteOn.velocity > 0 ? FERRITRON_EVENT_NOTE_ON : FERRITRON_EVENT_NOTE_OFF;
					ev.data.note = {e.noteOn.noteId, e.noteOn.channel, e.noteOn.pitch, e.noteOn.velocity};
				} else if (e.type == Event::kNoteOffEvent) {
					ev.type = FERRITRON_EVENT_NOTE_OFF;
					ev.data.note = {e.noteOff.noteId, e.noteOff.channel, e.noteOff.pitch, e.noteOff.velocity};
				} else if (e.type == Event::kNoteExpressionValueEvent) {
					// VST3 Note Expression: the standard types share CLAP's numbers (0 volume .. 5
					// brightness); values come normalized and go out in CLAP's units.
					const auto& x = e.noteExpressionValue;
					if (x.typeId > kBrightnessTypeID)
						continue;
					double v = x.value;
					if (x.typeId == kVolumeTypeID)
						v *= 4.0; // linear gain, 0.25 = 0 dB
					else if (x.typeId == kTuningTypeID)
						v = 240.0 * (v - 0.5); // semitones
					ev.type = FERRITRON_EVENT_NOTE_EXPRESSION;
					ev.data.voice.note_id = x.noteId;
					ev.data.voice.key = -1;
					ev.data.voice.channel = -1;
					ev.data.voice.target = x.typeId;
					ev.data.voice.value = static_cast<float>(v);
				} else if (e.type == Event::kPolyPressureEvent) {
					ev.type = FERRITRON_EVENT_MIDI;
					ev.data.midi.status = 0xA0;
					ev.data.midi.channel = static_cast<uint8_t>(e.polyPressure.channel & 15);
					ev.data.midi.data1 = static_cast<uint8_t>(e.polyPressure.pitch & 127);
					ev.data.midi.data2 = static_cast<uint8_t>(std::clamp(e.polyPressure.pressure, 0.0f, 1.0f) * 127.0f + 0.5f);
				} else if (e.type == Event::kLegacyMIDICCOutEvent) {
					const auto& cc = e.midiCCOut;
					ev.type = FERRITRON_EVENT_MIDI;
					ev.data.midi.channel = static_cast<uint8_t>(cc.channel & 15);
					if (cc.controlNumber < 128) {
						ev.data.midi.status = 0xB0;
						ev.data.midi.data1 = cc.controlNumber;
						ev.data.midi.data2 = static_cast<uint8_t>(cc.value & 127);
					} else if (cc.controlNumber == 128) {
						ev.data.midi.status = 0xD0;
						ev.data.midi.data1 = static_cast<uint8_t>(cc.value & 127);
					} else if (cc.controlNumber == 129) {
						ev.data.midi.status = 0xE0;
						ev.data.midi.data1 = static_cast<uint8_t>(cc.value & 127);
						ev.data.midi.data2 = static_cast<uint8_t>(cc.value2 & 127);
					} else if (cc.controlNumber == 130) {
						ev.data.midi.status = 0xC0;
						ev.data.midi.data1 = static_cast<uint8_t>(cc.value & 127);
					} else {
						continue;
					}
				} else {
					continue;
				}
				insert(ev);
			}
		}
		if (IParameterChanges* changes = data.inputParameterChanges) {
			for (int32 i = 0, n = changes->getParameterCount(); i < n; ++i) {
				IParamValueQueue* queue = changes->getParameterData(i);
				if (!queue)
					continue;
				ParamID id = queue->getParameterId();
				for (int32 p = 0, points = queue->getPointCount(); p < points; ++p) {
					int32 offset = 0;
					ParamValue value = 0;
					if (queue->getPoint(p, offset, value) != kResultOk)
						continue;
					if (numEvents == kMaxEvents) {
						// Overflow: keep it for after the block (notes already have their room); apply now if that is full too.
						if (numLate < kMaxLate) {
							ferritron_event& ev = late[numLate++];
							ev = ferritron_event{};
							ev.type = FERRITRON_EVENT_PARAM;
							ev.data.param.id = id;
							ev.data.param.normalized = value;
						} else {
							api->set_param(core, id, value);
						}
						continue;
					}
					ferritron_event ev{};
					ev.offset = static_cast<uint32_t>(std::max<int32>(offset, 0));
					if (midiMap && isMidiParam(id)) {
						// A controller the host mapped (IMidiMapping): becomes a MIDI message.
						const uint32_t ch = (id - kMidiParamBase) / 256, ctrl = (id - kMidiParamBase) % 256;
						const auto v7 = static_cast<uint8_t>(std::clamp(value, 0.0, 1.0) * 127.0 + 0.5);
						ev.type = FERRITRON_EVENT_MIDI;
						ev.data.midi.channel = static_cast<uint8_t>(ch);
						if (ctrl < 128) {
							ev.data.midi.status = 0xB0;
							ev.data.midi.data1 = static_cast<uint8_t>(ctrl);
							ev.data.midi.data2 = v7;
						} else if (ctrl == 128) {
							ev.data.midi.status = 0xD0;
							ev.data.midi.data1 = v7;
						} else if (ctrl == 129) {
							const auto n = static_cast<uint32_t>(std::clamp(value, 0.0, 1.0) * 16383.0 + 0.5);
							ev.data.midi.status = 0xE0;
							ev.data.midi.data1 = static_cast<uint8_t>(n & 0x7F);
							ev.data.midi.data2 = static_cast<uint8_t>(n >> 7);
						} else if (ctrl == 130) {
							ev.data.midi.status = 0xC0;
							ev.data.midi.data1 = v7;
						} else {
							continue;
						}
						insert(ev);
						continue;
					}
					ev.type = FERRITRON_EVENT_PARAM;
					ev.data.param.id = id;
					ev.data.param.normalized = value;
					insert(ev);
				}
			}
		}
		if (!ferritron_events_sorted(events, numEvents)) {
			ferritron_event scratch[kMaxEvents];
			ferritron_sort_events(events, scratch, numEvents);
		}
		// Overflowed parameter changes, applied in their order after the block.
		auto applyLate = [&] {
			for (uint32_t i = 0; i < numLate; ++i)
				api->set_param(core, late[i].data.param.id, late[i].data.param.normalized);
			numLate = 0;
		};
		auto flushEvents = [&] {
			for (uint32_t i = 0; i < numEvents; ++i)
				if (events[i].type == FERRITRON_EVENT_PARAM)
					api->set_param(core, events[i].data.param.id, events[i].data.param.normalized);
			applyLate();
		};

		// A call without audio only flushes parameters.
		if (data.numSamples <= 0 || data.numOutputs < 1 || !data.outputs) {
			flushEvents();
			return kResultOk;
		}
		if (data.symbolicSampleSize != kSample32) {
			flushEvents();
			return kResultFalse;
		}
		AudioBusBuffers& out = data.outputs[0];
		if (!out.channelBuffers32) {
			flushEvents();
			return kResultOk;
		}
		const AudioBusBuffers* in = data.numInputs > 0 && data.inputs ? &data.inputs[0] : nullptr;
		const int32 channels = std::min(out.numChannels, kChannels);
		const auto frames = static_cast<size_t>(data.numSamples);

		// Copy inputs to outputs so the plugin processes in place.
		float* ptrs[kChannels] = {};
		for (int32 ch = 0; ch < channels; ++ch) {
			float* dst = out.channelBuffers32[ch];
			if (!dst) {
				flushEvents();
				return kResultOk;
			}
			const float* src = in && in->channelBuffers32 && ch < in->numChannels ? in->channelBuffers32[ch] : nullptr;
			if (!src)
				std::memset(dst, 0, frames * sizeof(float));
			else if (src != dst)
				std::memmove(dst, src, frames * sizeof(float));
			ptrs[ch] = dst;
		}

		// The sidechain input, if the host connected it (read-only; the plugin treats a missing one as silence).
		if (sidechainApi) {
			if (hasSidechain && scActive && data.numInputs > 1 && data.inputs && data.inputs[1].channelBuffers32 && data.inputs[1].numChannels > 0)
				sidechainApi->set(core, data.inputs[1].channelBuffers32, static_cast<uint32_t>(std::min(data.inputs[1].numChannels, kChannels)));
			else
				sidechainApi->set(core, nullptr, 0);
		}

		// The host's transport (tempo, time signature, play state, position) for this block.
		if (transportApi) {
			ferritron_transport t{};
			if (const ProcessContext* c = data.processContext) {
				if (c->state & ProcessContext::kTempoValid) {
					t.flags |= FERRITRON_TRANSPORT_TEMPO;
					t.tempo = c->tempo;
				}
				if (c->state & ProcessContext::kTimeSigValid) {
					t.flags |= FERRITRON_TRANSPORT_TIME_SIGNATURE;
					t.numerator = static_cast<uint16_t>(c->timeSigNumerator);
					t.denominator = static_cast<uint16_t>(c->timeSigDenominator);
				}
				if (c->state & ProcessContext::kPlaying)
					t.flags |= FERRITRON_TRANSPORT_PLAYING;
				if (c->state & ProcessContext::kRecording)
					t.flags |= FERRITRON_TRANSPORT_RECORDING;
				if (c->state & ProcessContext::kProjectTimeMusicValid) {
					t.flags |= FERRITRON_TRANSPORT_POSITION_BEATS;
					t.position_beats = c->projectTimeMusic;
				}
				if (c->state & ProcessContext::kBarPositionValid) {
					t.flags |= FERRITRON_TRANSPORT_BAR_START;
					t.bar_start_beats = c->barPositionMusic;
				}
				t.flags |= FERRITRON_TRANSPORT_POSITION_SAMPLES;
				t.position_samples = c->projectTimeSamples;
				if (c->state & ProcessContext::kCycleValid) {
					t.flags |= FERRITRON_TRANSPORT_LOOP_RANGE;
					t.loop_start = c->cycleStartMusic;
					t.loop_end = c->cycleEndMusic;
				}
				if (c->state & ProcessContext::kCycleActive)
					t.flags |= FERRITRON_TRANSPORT_LOOPING;
			}
			transportApi->set(core, &t);
		}

		// An ARA playback renderer plays its regions at the song position; the plugin processes the result.
		if (ara) {
			const ProcessContext* context = data.processContext;
			bool playing = context && (context->state & ProcessContext::kPlaying);
			ara->render(core, ptrs, static_cast<uint32_t>(channels), static_cast<uint32_t>(frames),
			            context ? context->projectTimeSamples : 0, playing);
		}

		// The host says per block whether it renders offline (export): the
		// plugin's quality follows at once.
		const bool blockOffline = data.processMode == kOffline;
		if (renderExt && blockOffline != liveOffline) {
			liveOffline = blockOffline;
			renderExt->set_offline(core, blockOffline);
		}

		// VST3 has no host thread pool; Ferritron's own workers are used instead.
		bool silent = false;
		// Events the plugin writes (parameter moves, MIDI out) come back here.
		constexpr uint32_t kMaxOut = 256;
		ferritron_event outEvents[kMaxOut];
		uint32_t outCount = 0;
		const ferritron_process_data pd = {sizeof(pd), static_cast<uint32_t>(frames), static_cast<uint32_t>(channels), numEvents, ptrs, events, nullptr, outEvents, kMaxOut, &outCount};
		if (passing) {
			flushEvents(); // out of range: the input goes out unchanged
		} else if (!api->process(core, &pd)) {
			for (int32 ch = 0; ch < channels; ++ch)
				std::memset(ptrs[ch], 0, frames * sizeof(float));
		} else {
			silent = sleepExt && sleepExt->silent(core);
			sendOutEvents(data, outEvents, outCount);
		}
		applyLate();
		// A sleeping plugin outputs silence: say so, hosts can skip work downstream.
		out.silenceFlags = silent ? (uint64(1) << channels) - 1 : 0;
		return kResultOk;
	}

	// Hands what the plugin produced during the block to the host: parameter
	// moves as output parameter changes, MIDI as output events.
	void sendOutEvents(ProcessData& data, const ferritron_event* out, uint32_t count) {
		for (uint32_t i = 0; i < count; ++i) {
			const ferritron_event& ev = out[i];
			if (ev.type == FERRITRON_EVENT_PARAM && data.outputParameterChanges) {
				int32 index = 0;
				if (IParamValueQueue* queue = data.outputParameterChanges->addParameterData(ev.data.param.id, index)) {
					int32 point = 0;
					queue->addPoint(static_cast<int32>(ev.offset), ev.data.param.normalized, point);
				}
			} else if (ev.type == FERRITRON_EVENT_MIDI && data.outputEvents) {
				Event e{};
				e.busIndex = 0;
				e.sampleOffset = static_cast<int32>(ev.offset);
				const auto& m = ev.data.midi;
				const int16 channel = m.channel & 15;
				if (m.status == 0x90 || m.status == 0x80) {
					const bool on = m.status == 0x90 && m.data2 > 0;
					e.type = on ? Event::kNoteOnEvent : Event::kNoteOffEvent;
					if (on) {
						e.noteOn.channel = channel;
						e.noteOn.pitch = m.data1;
						e.noteOn.velocity = m.data2 / 127.0f;
						e.noteOn.noteId = -1;
					} else {
						e.noteOff.channel = channel;
						e.noteOff.pitch = m.data1;
						e.noteOff.velocity = m.data2 / 127.0f;
						e.noteOff.noteId = -1;
					}
				} else if (m.status == 0xA0) {
					e.type = Event::kPolyPressureEvent;
					e.polyPressure.channel = channel;
					e.polyPressure.pitch = m.data1;
					e.polyPressure.pressure = m.data2 / 127.0f;
					e.polyPressure.noteId = -1;
				} else {
					e.type = Event::kLegacyMIDICCOutEvent;
					e.midiCCOut.channel = static_cast<int8>(channel);
					e.midiCCOut.value = 0;
					e.midiCCOut.value2 = 0;
					if (m.status == 0xB0) {
						e.midiCCOut.controlNumber = m.data1;
						e.midiCCOut.value = static_cast<int8>(m.data2);
					} else if (m.status == 0xD0) {
						e.midiCCOut.controlNumber = 128;
						e.midiCCOut.value = static_cast<int8>(m.data1);
					} else if (m.status == 0xE0) {
						e.midiCCOut.controlNumber = 129;
						e.midiCCOut.value = static_cast<int8>(m.data1);
						e.midiCCOut.value2 = static_cast<int8>(m.data2);
					} else if (m.status == 0xC0) {
						e.midiCCOut.controlNumber = 130;
						e.midiCCOut.value = static_cast<int8>(m.data1);
					} else {
						continue;
					}
				}
				data.outputEvents->addEvent(e);
			}
		}
	}

	uint32 PLUGIN_API getTailSamples() override {
		if (!sleepExt)
			return kNoTail;
		uint32_t tail = sleepExt->tail(core);
		return tail == FERRITRON_TAIL_UNKNOWN ? kInfiniteTail : tail;
	}

	// IEditController
	tresult PLUGIN_API setComponentState(IBStream* state) override {
		// One object: already applied through IComponent::setState. A controller of its own
		// (`[vst3] controller_id`) is a second instance, which takes the component's state here.
		if (separateController && peer)
			return kResultOk; // the processor took the state in IComponent::setState
		return separateController ? setComponentStateImpl(state) : kResultOk;
	}

	/// This object was created as the controller class (`[vst3] controller_id`).
	bool separateController = false;

	tresult setControllerStateImpl(IBStream*) override { return kResultOk; }
	tresult getControllerStateImpl(IBStream*) override { return kResultOk; }

	int32 PLUGIN_API getParameterCount() override {
		return static_cast<int32>(api->param_count()) + (midiMap ? kMidiParamCount : 0);
	}

	tresult PLUGIN_API getParameterInfo(int32 index, ParameterInfo& out) override {
		const int32 own = static_cast<int32>(api->param_count());
		if (midiMap && index >= own && index < own + kMidiParamCount) {
			const int32 i = index - own;
			const int32 ch = i / kMidiCtrls, ctrl = i % kMidiCtrls;
			out.id = kMidiParamBase + static_cast<ParamID>(ch) * 256 + static_cast<ParamID>(ctrl);
			char title[64];
			if (ctrl < 128)
				std::snprintf(title, sizeof(title), "MIDI Ch%d CC%d", ch + 1, ctrl);
			else
				std::snprintf(title, sizeof(title), "MIDI Ch%d %s", ch + 1, ctrl == 128 ? "Pressure" : ctrl == 129 ? "Pitch Bend" : "Program");
			copyUtf16(out.title, 128, title, std::strlen(title));
			copyUtf16(out.shortTitle, 128, title, std::strlen(title));
			copyUtf16(out.units, 128, "", 0);
			out.stepCount = 0;
			out.defaultNormalizedValue = ctrl == 129 ? 0.5 : 0.0;
			out.unitId = kRootUnitId;
			out.flags = ParameterInfo::kIsHidden;
			return kResultOk;
		}
		ferritron_param_info info{};
		if (index < 0 || !api->param_info(static_cast<uint32_t>(index), &info))
			return kInvalidArgument;
		out.id = info.id;
		copyUtf16(out.title, 128, info.name);
		copyUtf16(out.shortTitle, 128, info.name);
		copyUtf16(out.units, 128, info.unit);
		out.stepCount = static_cast<int32>(info.step_count);
		out.defaultNormalizedValue = info.default_normalized;
		out.unitId = kRootUnitId;
		out.flags = (info.flags & FERRITRON_PARAM_NOT_AUTOMATABLE) ? 0 : ParameterInfo::kCanAutomate;
		if (info.flags & FERRITRON_PARAM_LIST)
			out.flags |= ParameterInfo::kIsList;
		if (info.flags & FERRITRON_PARAM_BYPASS)
			out.flags |= ParameterInfo::kIsBypass;
		if (info.flags & FERRITRON_PARAM_PROGRAM)
			out.flags = ParameterInfo::kIsProgramChange | ParameterInfo::kIsList; // shown in the host's program list
		else if (info.flags & FERRITRON_PARAM_HIDDEN)
			out.flags |= ParameterInfo::kIsHidden;
		return kResultOk;
	}

	// IUnitInfo (plugins with [presets]): one root unit with one program list, the factory
	// presets. The list's program-change parameter is Ferritron's built-in Program parameter;
	// setting it loads the preset on the main thread.
	static constexpr ProgramListID kProgramListId = 1;

	// Finds the Program parameter once (constructor).
	void findProgramParam() {
		const uint32_t n = api->param_count();
		for (uint32_t i = 0; i < n; ++i) {
			ferritron_param_info info{};
			if (api->param_info(i, &info) && (info.flags & FERRITRON_PARAM_PROGRAM)) {
				programParam = info.id;
				programCount = static_cast<int32>(info.step_count) + 1;
				return;
			}
		}
	}

	int32 PLUGIN_API getUnitCount() override { return 1; }

	tresult PLUGIN_API getUnitInfo(int32 unitIndex, UnitInfo& info) override {
		if (unitIndex != 0)
			return kInvalidArgument;
		info.id = kRootUnitId;
		info.parentUnitId = kNoParentUnitId;
		copyUtf16(info.name, 128, "Root", 4);
		info.programListId = programCount > 0 ? kProgramListId : kNoProgramListId;
		return kResultOk;
	}

	int32 PLUGIN_API getProgramListCount() override { return programCount > 0 ? 1 : 0; }

	tresult PLUGIN_API getProgramListInfo(int32 listIndex, ProgramListInfo& info) override {
		if (listIndex != 0 || programCount <= 0)
			return kInvalidArgument;
		info.id = kProgramListId;
		copyUtf16(info.name, 128, "Factory", 7);
		info.programCount = programCount;
		return kResultOk;
	}

	tresult PLUGIN_API getProgramName(ProgramListID listId, int32 programIndex, String128 name) override {
		if (listId != kProgramListId || programIndex < 0 || programIndex >= programCount)
			return kInvalidArgument;
		char buf[256];
		int32_t len = api->format_param(programParam, api->to_normalized(programParam, programIndex), buf, sizeof(buf));
		if (len < 0)
			return kResultFalse;
		copyUtf16(name, 128, buf, static_cast<size_t>(len));
		return kResultOk;
	}

	tresult PLUGIN_API getProgramInfo(ProgramListID, int32, Steinberg::Vst::CString, String128) override { return kResultFalse; }
	tresult PLUGIN_API hasProgramPitchNames(ProgramListID, int32) override { return kResultFalse; }
	tresult PLUGIN_API getProgramPitchName(ProgramListID, int32, int16, String128) override { return kResultFalse; }
	UnitID PLUGIN_API getSelectedUnit() override { return kRootUnitId; }
	tresult PLUGIN_API selectUnit(UnitID) override { return kResultOk; }
	tresult PLUGIN_API getUnitByBus(MediaType, BusDirection, int32, int32, UnitID& unitId) override {
		unitId = kRootUnitId;
		return kResultOk;
	}
	tresult PLUGIN_API setUnitProgramData(int32, int32, IBStream*) override { return kNotImplemented; }

	ParamID programParam = 0;
	int32 programCount = 0;

	// INoteExpressionController (instruments that say how many voices they play,
	// Process::VOICES): the standard expressions, volume to brightness.
	uint32_t voices = 0;

	int32 PLUGIN_API getNoteExpressionCount(int32 busIndex, int16) override { return voices > 0 && busIndex == 0 ? kBrightnessTypeID + 1 : 0; }

	tresult PLUGIN_API getNoteExpressionInfo(int32 busIndex, int16, int32 index, NoteExpressionTypeInfo& info) override {
		if (voices == 0 || busIndex != 0 || index < 0 || index > static_cast<int32>(kBrightnessTypeID))
			return kInvalidArgument;
		static const char* const titles[] = {"Volume", "Pan", "Tuning", "Vibrato", "Expression", "Brightness"};
		static const char* const units[] = {"", "", "st", "", "", ""};
		info = NoteExpressionTypeInfo{};
		info.typeId = static_cast<NoteExpressionTypeID>(index);
		copyUtf16(info.title, 128, titles[index], std::strlen(titles[index]));
		copyUtf16(info.shortTitle, 128, titles[index], std::strlen(titles[index]));
		copyUtf16(info.units, 128, units[index], std::strlen(units[index]));
		info.unitId = kRootUnitId;
		info.valueDesc.minimum = 0.0;
		info.valueDesc.maximum = 1.0;
		info.valueDesc.defaultValue = index == kVolumeTypeID ? 0.25 : (index == kPanTypeID || index == kTuningTypeID) ? 0.5 : 0.0;
		info.valueDesc.stepCount = 0;
		info.associatedParameterId = kNoParamId;
		info.flags = (index == kPanTypeID || index == kTuningTypeID) ? NoteExpressionTypeInfo::kIsBipolar : 0;
		return kResultOk;
	}

	tresult PLUGIN_API getNoteExpressionStringByValue(int32, int16, NoteExpressionTypeID id, NoteExpressionValue v, String128 string) override {
		char buf[64];
		if (id == kVolumeTypeID)
			std::snprintf(buf, sizeof(buf), v <= 0.0 ? "-inf dB" : "%.1f dB", 20.0 * std::log10(4.0 * v));
		else if (id == kTuningTypeID)
			std::snprintf(buf, sizeof(buf), "%+.2f st", 240.0 * (v - 0.5));
		else if (id <= kBrightnessTypeID)
			std::snprintf(buf, sizeof(buf), "%.0f%%", v * 100.0);
		else
			return kInvalidArgument;
		copyUtf16(string, 128, buf, std::strlen(buf));
		return kResultOk;
	}

	tresult PLUGIN_API getNoteExpressionValueByString(int32, int16, NoteExpressionTypeID id, const TChar* string, NoteExpressionValue& value) override {
		std::string text = toUtf8(const_cast<TChar*>(string));
		double x = 0;
		if (id > kBrightnessTypeID || std::sscanf(text.c_str(), "%lf", &x) != 1)
			return kResultFalse;
		if (id == kVolumeTypeID)
			value = std::pow(10.0, x / 20.0) / 4.0;
		else if (id == kTuningTypeID)
			value = x / 240.0 + 0.5;
		else
			value = x / 100.0;
		value = std::clamp(value, 0.0, 1.0);
		return kResultOk;
	}

	tresult PLUGIN_API getParamStringByValue(ParamID id, ParamValue value, String128 string) override {
		char buf[256];
		int32_t len = api->format_param(id, value, buf, sizeof(buf));
		if (len < 0)
			return kInvalidArgument;
		copyUtf16(string, 128, buf, static_cast<size_t>(len));
		return kResultOk;
	}

	tresult PLUGIN_API getParamValueByString(ParamID id, TChar* string, ParamValue& value) override {
		std::string text = toUtf8(string);
		ferritron_str s{text.data(), static_cast<uint32_t>(text.size())};
		return api->parse_param(id, s, &value) ? kResultOk : kResultFalse;
	}

	ParamValue PLUGIN_API normalizedParamToPlain(ParamID id, ParamValue value) override {
		return api->to_plain(id, value);
	}

	ParamValue PLUGIN_API plainParamToNormalized(ParamID id, ParamValue value) override {
		return api->to_normalized(id, value);
	}

	ParamValue PLUGIN_API getParamNormalized(ParamID id) override {
		if (isMidiParam(id))
			return id % 256 == 129 ? 0.5 : 0.0;
		Plugin* values = valuesOwner();
		return values->api->get_param(values->core, id);
	}

	tresult PLUGIN_API setParamNormalized(ParamID id, ParamValue value) override {
		if (isMidiParam(id))
			return kResultOk; // controllers are events, not state
		Plugin* values = valuesOwner();
		return values->api->set_param(values->core, id, value) ? kResultOk : kInvalidArgument;
	}

	tresult PLUGIN_API setComponentHandler(IComponentHandler* next) override {
		if (next)
			next->addRef();
		if (handler)
			handler->release();
		handler = next;
		return kResultOk;
	}

	// Without a view extension hosts show their generic parameter UI.
	IPlugView* PLUGIN_API createView(FIDString name) override;

	// Edits coming from the plugin's own editor: apply them and tell the host,
	// so it can record automation.
	double editorGet(ParamID id) { return api->get_param(core, id); }
	void editorBegin(ParamID id) {
		if (IComponentHandler* h = hostHandler())
			h->beginEdit(id);
	}
	void editorPerform(ParamID id, ParamValue value) {
		api->set_param(core, id, value);
		if (IComponentHandler* h = hostHandler())
			h->performEdit(id, api->get_param(core, id));
	}
	void editorEnd(ParamID id) {
		if (IComponentHandler* h = hostHandler())
			h->endEdit(id);
	}

	// The host's handler: this object's, else the connected partner's (`[vst3] controller_id`: the host
	// gives the handler to the controller, the editor's edits come from the processor's instance).
	IComponentHandler* hostHandler() const { return handler ? handler : (peer ? peer->handler : nullptr); }

	// The processor's object when this is a connected controller: it owns the parameter values.
	Plugin* valuesOwner() { return separateController && peer ? peer : this; }

	// IConnectionPoint: the host connects the processor and the controller of a `[vst3] controller_id`
	// plugin. Only objects of this plugin in this process are connected (a host that puts a proxy between
	// them gets no link: the parts then run apart, as before).
	tresult PLUGIN_API connect(IConnectionPoint* other) override {
		if (!other)
			return kInvalidArgument;
		Plugin* that = nullptr;
		if (other->queryInterface(kPeerIid, reinterpret_cast<void**>(&that)) != kResultOk || !that)
			return kResultFalse;
		that->release(); // the reference queryInterface added; the host keeps its own
		if (that == this || that->api != api || separateController == that->separateController || (peer && peer != that) ||
		    (that->peer && that->peer != this))
			return kResultFalse;
		if (peer == that)
			return kResultOk;
		peer = that;
		that->peer = this;
		// The controller holds the processor while they are connected (one reference, no cycle).
		Plugin* controller = separateController ? this : that;
		Plugin* processor = separateController ? that : this;
		if (!controller->peerOwned) {
			processor->addRef();
			controller->peerOwned = true;
		}
		return kResultOk;
	}

	tresult PLUGIN_API disconnect(IConnectionPoint*) override {
		unpeer();
		return kResultOk;
	}

	tresult PLUGIN_API notify(IMessage*) override { return kResultOk; }

	// Clears the link on both sides; the controller's reference to the processor goes with it.
	void unpeer() {
		Plugin* that = peer;
		if (!that)
			return;
		peer = nullptr;
		if (that->peer == this)
			that->peer = nullptr;
		Plugin* controller = separateController ? this : that;
		Plugin* processor = separateController ? that : this;
		if (controller->peerOwned) {
			controller->peerOwned = false;
			processor->release(); // may destroy `this`: nothing of it is used after this
		}
	}

	// The processor (or the other half) this object is connected to, if any.
	Plugin* peer = nullptr;
	bool peerOwned = false;

	// Editor size in logical pixels; kept while the plugin lives, so a
	// reopened editor has the size the user chose.
	uint32_t editorWidth = 0;
	uint32_t editorHeight = 0;

	// IProcessContextRequirements
	uint32 PLUGIN_API getProcessContextRequirements() override {
		return transportApi ? (IProcessContextRequirements::kNeedTempo | IProcessContextRequirements::kNeedTimeSignature |
		                       IProcessContextRequirements::kNeedProjectTimeMusic | IProcessContextRequirements::kNeedBarPositionMusic |
		                       IProcessContextRequirements::kNeedTransportState | IProcessContextRequirements::kNeedCycleMusic)
		                    : 0;
	}

private:
	const ferritron_plugin_api* api;
	void* core;
	const ferritron_ara_api* ara = araApi(api);
	const ferritron_render_api* renderExt = static_cast<const ferritron_render_api*>(api->get_extension(FERRITRON_EXT_RENDER));
	const ferritron_sleep_api* sleepExt = static_cast<const ferritron_sleep_api*>(api->get_extension(FERRITRON_EXT_SLEEP));
	bool liveOffline = false; // last told to the plugin from process()
	int32 busChannels = 2;
	const bool instrument;
	std::atomic<uint32> refs{1};
	IComponentHandler* handler = nullptr;
	const ferritron_host_api* link = nullptr;
	const ferritron_track_api* track = nullptr;
	const ferritron_transport_api* transportApi = nullptr;
	const ferritron_main_api* mainApi = nullptr;
	const ferritron_sidechain_api* sidechainApi = nullptr;
	const ferritron_layout_api* layoutApi = nullptr;
	bool hasSidechain = false;
	bool midiIn = false;
	bool midiMap = false;
	bool passthrough = false;
	bool scActive = false;
	bool passing = false; // an arrangement outside the plugin's range: the audio passes through
	int32 minCh = 1, maxCh = 2, scFixed = 0, scChannels = 2;
	SpeakerArrangement mainArr = SpeakerArr::kStereo;
	SpeakerArrangement scArr = SpeakerArr::kStereo;
	SampleRate sampleRate = 44100.0;
	int32 maxBlockSize = 1024;
	uint32 latency = 0;
	bool latencyPending = false; // a latency change from setActive, reported by the next pollMain
	bool offline = false;
};

// --- Editor ------------------------------------------------------------------

// IPlugView on top of the ferritron.view.v1 extension. Sizes reported to the
// host are physical pixels: the logical size times the scale factor, which is
// the host's (IPlugViewContentScaleSupport) or else the window's DPI.
namespace host_edits {
void deliver(Plugin* plugin, uint32_t kind, uint32_t id, double normalized) { plugin->hostEdited(kind, id, normalized); }

// Any thread: queued, then handed to the host on the main thread in order.
void push(Plugin* plugin, uint32_t kind, uint32_t id, double normalized) {
	std::lock_guard<std::mutex> guard(lock);
	queue.push_back({plugin, kind, id, normalized});
#if defined(__APPLE__)
	if (queue.size() == 1)
		dispatch_async_f(dispatch_get_main_queue(), nullptr, drain);
#elif !defined(_WIN32)
	// No main-thread timer here yet: deliver right away.
	for (auto& e : queue)
		deliver(e.plugin, e.kind, e.id, e.normalized);
	queue.clear();
#endif
}
#ifdef _WIN32
void CALLBACK tick(HWND, UINT, UINT_PTR, DWORD) {
	drain(nullptr);
}
#endif
// Main thread only. True while `plugin` is still registered. A call into the host (restartComponent,
// performEdit) may make the host destroy an instance, so the check is made again before each call.
bool alive(Plugin* plugin) {
	std::lock_guard<std::mutex> guard(lock);
	return std::find(registry.begin(), registry.end(), plugin) != registry.end();
}
#if defined(_WIN32) || defined(__APPLE__)
void drain(void*) {
	std::vector<Edit> pending;
	std::vector<Plugin*> live;
	{
		std::lock_guard<std::mutex> guard(lock);
		pending.swap(queue);
		live = registry;
	}
	// Plugins are created and destroyed on this thread. A plugin the host destroys during the
	// poll is skipped from then on (its edits were removed from the queue by ~Plugin).
	for (Plugin* plugin : live) {
		if (alive(plugin))
			plugin->pollMain();
	}
	for (auto& e : pending) {
		// Checked under the lock, then delivered outside it: deliver() calls the host, and the
		// host may take the same lock or destroy the plugin. Nothing else destroys a plugin
		// between this check and the call, because drain and ~Plugin run on the same thread.
		if (alive(e.plugin))
			deliver(e.plugin, e.kind, e.id, e.normalized);
	}
}
#endif
} // namespace host_edits

class Editor final : public IPlugView, public IPlugViewContentScaleSupport {
public:
	Editor(Plugin* plugin, const ferritron_view_api* api) : plugin(plugin), viewApi(api) {
		plugin->addRef();
		host.ctx = plugin;
		host.get_param = [](void* ctx, uint32_t id) { return static_cast<Plugin*>(ctx)->editorGet(id); };
		host.begin_edit = [](void* ctx, uint32_t id) { static_cast<Plugin*>(ctx)->editorBegin(id); };
		host.request_resize = [](void* ctx, uint32_t w, uint32_t h) -> bool {
			auto* open = static_cast<Plugin*>(ctx)->openEditor;
			if (!open || !open->frame)
				return false;
			ViewRect size(0, 0, static_cast<int32>(w), static_cast<int32>(h));
			return open->frame->resizeView(open, &size) == kResultOk;
		};
		host.perform_edit = [](void* ctx, uint32_t id, double v) { static_cast<Plugin*>(ctx)->editorPerform(id, v); };
		host.end_edit = [](void* ctx, uint32_t id) { static_cast<Plugin*>(ctx)->editorEnd(id); };
		uint32_t w = 0, h = 0;
		if (plugin->savedEditorSize(w, h)) {
			plugin->editorWidth = std::clamp(w, api->min_width, api->max_width);
			plugin->editorHeight = std::clamp(h, api->min_height, api->max_height);
		} else if (plugin->editorWidth == 0) {
			plugin->editorWidth = api->width;
			plugin->editorHeight = api->height;
		}
	}

	~Editor() {
		removed();
		plugin->release();
	}

	// State with another size was loaded while the editor is open.
	void restored() {
		if (!view)
			return;
		ViewRect size = physicalSize();
		if (frame)
			frame->resizeView(this, &size); // the host answers with onSize
		viewApi->set_size(view, static_cast<uint32_t>(size.getWidth()), static_cast<uint32_t>(size.getHeight()), scale());
	}

	tresult PLUGIN_API queryInterface(const TUID requested, void** obj) override {
		if (!obj)
			return kInvalidArgument;
		if (sameIid(requested, FUnknown_iid) || sameIid(requested, IPlugView_iid))
			*obj = static_cast<IPlugView*>(this);
		else if (sameIid(requested, IPlugViewContentScaleSupport_iid))
			*obj = static_cast<IPlugViewContentScaleSupport*>(this);
		else {
			*obj = nullptr;
			return kNoInterface;
		}
		addRef();
		return kResultOk;
	}

	uint32 PLUGIN_API addRef() override { return ++refs; }

	uint32 PLUGIN_API release() override {
		uint32 remaining = --refs;
		if (remaining == 0)
			delete this;
		return remaining;
	}

	tresult PLUGIN_API isPlatformTypeSupported(FIDString type) override {
#if SMTG_OS_WINDOWS
		return type && std::strcmp(type, kPlatformTypeHWND) == 0 ? kResultTrue : kResultFalse;
#elif SMTG_OS_MACOS
		return type && std::strcmp(type, kPlatformTypeNSView) == 0 ? kResultTrue : kResultFalse;
#else
		(void)type;
		return kResultFalse;
#endif
	}

	tresult PLUGIN_API attached(void* parentWindow, FIDString type) override {
		if (view || isPlatformTypeSupported(type) != kResultTrue)
			return kResultFalse;
		parent = parentWindow;
		ViewRect size = physicalSize();
		view = viewApi->open(parent, &host, static_cast<uint32_t>(size.getWidth()), static_cast<uint32_t>(size.getHeight()), scale());
		if (!view)
			return kResultFalse;
		plugin->openEditor = this;
		// The window's DPI may differ from what getSize assumed: ask for the right size.
		if (frame && (size.getWidth() != reported.getWidth() || size.getHeight() != reported.getHeight()))
			frame->resizeView(this, &size);
		return kResultOk;
	}

	tresult PLUGIN_API removed() override {
		if (plugin->openEditor == this)
			plugin->openEditor = nullptr;
		if (view)
			viewApi->close(view);
		view = nullptr;
		parent = nullptr;
		return kResultOk;
	}

	tresult PLUGIN_API onWheel(float) override { return kResultFalse; }
	tresult PLUGIN_API onKeyDown(char16, int16, int16) override { return kResultFalse; }
	tresult PLUGIN_API onKeyUp(char16, int16, int16) override { return kResultFalse; }

	tresult PLUGIN_API getSize(ViewRect* size) override {
		if (!size)
			return kInvalidArgument;
		*size = reported = physicalSize();
		return kResultOk;
	}

	tresult PLUGIN_API onSize(ViewRect* newSize) override {
		if (!newSize)
			return kInvalidArgument;
		ViewRect size = *newSize;
		checkSizeConstraint(&size);
		double s = scale();
		plugin->editorWidth = static_cast<uint32_t>(std::lround(size.getWidth() / s));
		plugin->editorHeight = static_cast<uint32_t>(std::lround(size.getHeight() / s));
		plugin->saveEditorSize();
		reported = size;
		if (view)
			viewApi->set_size(view, static_cast<uint32_t>(size.getWidth()), static_cast<uint32_t>(size.getHeight()), s);
		return kResultOk;
	}

	tresult PLUGIN_API onFocus(TBool) override { return kResultOk; }

	tresult PLUGIN_API setFrame(IPlugFrame* newFrame) override {
		frame = newFrame;
		return kResultOk;
	}

	bool resizable() const {
		return viewApi->min_width != viewApi->max_width || viewApi->min_height != viewApi->max_height;
	}

	tresult PLUGIN_API canResize() override { return resizable() ? kResultTrue : kResultFalse; }

	tresult PLUGIN_API checkSizeConstraint(ViewRect* rect) override {
		if (!rect)
			return kInvalidArgument;
		double s = scale();
		auto clamp = [s](int32 v, uint32_t lo, uint32_t hi) {
			return std::clamp(v, static_cast<int32>(std::lround(lo * s)), static_cast<int32>(std::lround(hi * s)));
		};
		rect->right = rect->left + clamp(rect->getWidth(), viewApi->min_width, viewApi->max_width);
		rect->bottom = rect->top + clamp(rect->getHeight(), viewApi->min_height, viewApi->max_height);
		return kResultTrue;
	}

	// IPlugViewContentScaleSupport
	tresult PLUGIN_API setContentScaleFactor(ScaleFactor factor) override {
		if (!(factor > 0))
			return kResultFalse;
		hostScale = factor;
		if (view) {
			ViewRect size = physicalSize();
			if (frame)
				frame->resizeView(this, &size); // the host answers with onSize
			viewApi->set_size(view, static_cast<uint32_t>(size.getWidth()), static_cast<uint32_t>(size.getHeight()), scale());
		}
		return kResultOk;
	}

private:
	// macOS sizes are points; the view itself renders at the screen's backing scale.
	double scale() const {
#if SMTG_OS_MACOS
		return 1.0;
#endif
		if (hostScale > 0)
			return hostScale;
#if SMTG_OS_WINDOWS
		UINT dpi = parent ? GetDpiForWindow(static_cast<HWND>(parent)) : GetDpiForSystem();
		return dpi ? dpi / 96.0 : 1.0;
#else
		return 1.0;
#endif
	}

	ViewRect physicalSize() const {
		double s = scale();
		return ViewRect(0, 0, static_cast<int32>(std::lround(plugin->editorWidth * s)),
		                static_cast<int32>(std::lround(plugin->editorHeight * s)));
	}

	Plugin* plugin;
	const ferritron_view_api* viewApi;
	ferritron_view_host host{};
	void* view = nullptr;
	void* parent = nullptr;
	IPlugFrame* frame = nullptr;
	double hostScale = 0; // 0: not given by the host
	ViewRect reported{};
	std::atomic<uint32> refs{1};
};

void Plugin::restoreEditorSize() {
	uint32_t w = 0, h = 0;
	auto* view = static_cast<const ferritron_view_api*>(api->get_extension(FERRITRON_EXT_VIEW));
	if (!view || !savedEditorSize(w, h))
		return;
	w = std::clamp(w, view->min_width, view->max_width);
	h = std::clamp(h, view->min_height, view->max_height);
	if (w == editorWidth && h == editorHeight)
		return;
	editorWidth = w;
	editorHeight = h;
	if (openEditor)
		openEditor->restored();
}

IPlugView* PLUGIN_API Plugin::createView(FIDString name) {
	if (!name || std::strcmp(name, ViewType::kEditor) != 0)
		return nullptr;
	auto* viewApi = static_cast<const ferritron_view_api*>(api->get_extension(FERRITRON_EXT_VIEW));
	// A connected controller opens the processor's editor: that instance has the values, streams and messages.
	Plugin* owner = separateController && peer ? peer : this;
	return viewApi ? new Editor(owner, viewApi) : nullptr;
}

// --- Factory -----------------------------------------------------------------

#ifdef FERRITRON_HAVE_ARA
// The "ARA Main Factory Class" hosts look for before creating instances.
class AraMainFactory final : public ARA::IMainFactory {
public:
	explicit AraMainFactory(const ferritron_ara_api* ara) : ara(ara) {}
	tresult PLUGIN_API queryInterface(const TUID requested, void** obj) override {
		if (!obj)
			return kInvalidArgument;
		if (sameIid(requested, FUnknown_iid) || sameIid(requested, ARA::IMainFactory_iid)) {
			*obj = static_cast<ARA::IMainFactory*>(this);
			addRef();
			return kResultOk;
		}
		*obj = nullptr;
		return kNoInterface;
	}
	uint32 PLUGIN_API addRef() override { return ++refs; }
	uint32 PLUGIN_API release() override {
		uint32 remaining = --refs;
		if (remaining == 0)
			delete this;
		return remaining;
	}
	const ARA::ARAFactory* PLUGIN_API getFactory() override { return static_cast<const ARA::ARAFactory*>(ara->get_factory()); }

private:
	const ferritron_ara_api* ara;
	std::atomic<uint32> refs{1};
};
#endif

// The "Plugin Compatibility Class" (VST 3.7.5): for hosts that find no moduleinfo.json in the
// bundle, it writes the same `Compatibility` list as JSON: this plug-in's class takes over
// the classes in `[vst3] compatibility = { replaces = [...] }`.
class Compatibility final : public IPluginCompatibility {
public:
	explicit Compatibility(const uint8_t* classId) { std::memcpy(id, classId, sizeof(id)); }

	tresult PLUGIN_API queryInterface(const TUID requested, void** obj) override {
		if (!obj)
			return kInvalidArgument;
		if (sameIid(requested, FUnknown_iid) || sameIid(requested, IPluginCompatibility_iid)) {
			*obj = static_cast<IPluginCompatibility*>(this);
			addRef();
			return kResultOk;
		}
		*obj = nullptr;
		return kNoInterface;
	}
	uint32 PLUGIN_API addRef() override { return ++refs; }
	uint32 PLUGIN_API release() override {
		uint32 remaining = --refs;
		if (remaining == 0)
			delete this;
		return remaining;
	}

	tresult PLUGIN_API getCompatibilityJSON(IBStream* stream) override {
		std::string json = "[";
		if (!options().replaces.empty()) {
			json += "{\"New\":\"" + uidString(id) + "\",\"Old\":[";
			bool first = true;
			for (const auto& old : options().replaces) {
				if (!first)
					json += ",";
				first = false;
				json += "\"" + plainHex(old.data()) + "\"";
			}
			json += "]}";
		}
		json += "]";
		return writeAll(stream, std::vector<uint8_t>(json.begin(), json.end())) ? kResultTrue : kResultFalse;
	}

private:
	uint8_t id[16];
	std::atomic<uint32> refs{1};
};

class Factory final : public IPluginFactory3 {
public:
	explicit Factory(const ferritron_plugin_api* api) : api(api), info(*api->info) {}

	tresult PLUGIN_API queryInterface(const TUID requested, void** obj) override {
		if (!obj)
			return kInvalidArgument;
		if (sameIid(requested, FUnknown_iid) || sameIid(requested, IPluginFactory_iid) || sameIid(requested, IPluginFactory2_iid) ||
		    sameIid(requested, IPluginFactory3_iid)) {
			*obj = this;
			return kResultOk;
		}
		*obj = nullptr;
		return kNoInterface;
	}

	// The factory is a static object; reference counting is a no-op.
	uint32 PLUGIN_API addRef() override { return 1; }
	uint32 PLUGIN_API release() override { return 1; }

	tresult PLUGIN_API getFactoryInfo(PFactoryInfo* out) override {
		if (!out)
			return kInvalidArgument;
		copyUtf8(out->vendor, PFactoryInfo::kNameSize, info.vendor);
		copyUtf8(out->url, PFactoryInfo::kURLSize, info.url);
		copyUtf8(out->email, PFactoryInfo::kEmailSize, info.email);
		out->flags = PFactoryInfo::kUnicode;
		return kResultOk;
	}

	// Class 0 is the component; then ARA's main factory (with ARA), the edit controller
	// (`[vst3] controller_id`) and the compatibility class (`[vst3] compatibility`).
	enum class Kind { Audio, Ara, Controller, Compat, None };
	Kind kindAt(int32 index) const {
		int32 i = 0;
		if (index == i++)
			return Kind::Audio;
		if (araApi(api) && index == i++)
			return Kind::Ara;
		if (options().controller && index == i++)
			return Kind::Controller;
		if (options().compat && index == i++)
			return Kind::Compat;
		return Kind::None;
	}

	int32 PLUGIN_API countClasses() override {
		return 1 + (araApi(api) ? 1 : 0) + (options().controller ? 1 : 0) + (options().compat ? 1 : 0);
	}

	// Class 1 (with ARA): the ARA main factory. Its ID is the plugin's with the last byte flipped.
	void araClassId(TUID out) const {
		std::memcpy(out, info.class_id, sizeof(TUID));
		out[15] = static_cast<char>(out[15] ^ 0xA5);
	}
	template <typename Info, typename SetName>
	tresult araClassInfo(Info* out, SetName setName) {
		araClassId(out->cid);
		out->cardinality = PClassInfo::kManyInstances;
#ifdef FERRITRON_HAVE_ARA
		copyUtf8(out->category, PClassInfo::kCategorySize, str(kARAMainFactoryClass));
#endif
		setName(out);
		return kResultOk;
	}
	// The controller and compatibility classes (same fields as the ARA class, other IDs).
	template <typename Info, typename SetName>
	tresult extraClassInfo(Info* out, Kind kind, SetName setName) {
		std::memcpy(out->cid, kind == Kind::Controller ? options().controllerId : options().compatId, sizeof(TUID));
		out->cardinality = PClassInfo::kManyInstances;
		copyUtf8(out->category, PClassInfo::kCategorySize, str(kind == Kind::Controller ? kVstComponentControllerClass : kPluginCompatibilityClass));
		setName(out);
		return kResultOk;
	}

	tresult PLUGIN_API getClassInfo(int32 index, PClassInfo* out) override {
		if (out && kindAt(index) == Kind::Ara)
			return araClassInfo(out, [&](PClassInfo* o) { copyUtf8(o->name, PClassInfo::kNameSize, info.name); });
		if (out && (kindAt(index) == Kind::Controller || kindAt(index) == Kind::Compat))
			return extraClassInfo(out, kindAt(index), [&](PClassInfo* o) { copyUtf8(o->name, PClassInfo::kNameSize, str(extraName(kindAt(index)).c_str())); });
		if (index != 0 || !out)
			return kInvalidArgument;
		std::memcpy(out->cid, info.class_id, sizeof(TUID));
		out->cardinality = PClassInfo::kManyInstances;
		copyUtf8(out->category, PClassInfo::kCategorySize, str(kVstAudioEffectClass));
		copyUtf8(out->name, PClassInfo::kNameSize, info.name);
		return kResultOk;
	}

	tresult PLUGIN_API getClassInfo2(int32 index, PClassInfo2* out) override {
		if (out && (kindAt(index) == Kind::Controller || kindAt(index) == Kind::Compat))
			return extraClassInfo(out, kindAt(index), [&](PClassInfo2* o) {
				copyUtf8(o->name, PClassInfo::kNameSize, str(extraName(kindAt(index)).c_str()));
				o->classFlags = 0;
				copyUtf8(o->subCategories, PClassInfo2::kSubCategoriesSize, str(""));
				copyUtf8(o->vendor, PClassInfo2::kVendorSize, info.vendor);
				copyUtf8(o->version, PClassInfo2::kVersionSize, info.version);
				copyUtf8(o->sdkVersion, PClassInfo2::kVersionSize, str(kVstVersionString));
			});
		if (out && kindAt(index) == Kind::Ara)
			return araClassInfo(out, [&](PClassInfo2* o) {
				copyUtf8(o->name, PClassInfo::kNameSize, info.name);
				o->classFlags = 0;
				copyUtf8(o->subCategories, PClassInfo2::kSubCategoriesSize, str(""));
				copyUtf8(o->vendor, PClassInfo2::kVendorSize, info.vendor);
				copyUtf8(o->version, PClassInfo2::kVersionSize, info.version);
				copyUtf8(o->sdkVersion, PClassInfo2::kVersionSize, str(kVstVersionString));
			});
		if (index != 0 || !out)
			return kInvalidArgument;
		std::memcpy(out->cid, info.class_id, sizeof(TUID));
		out->cardinality = PClassInfo::kManyInstances;
		copyUtf8(out->category, PClassInfo::kCategorySize, str(kVstAudioEffectClass));
		copyUtf8(out->name, PClassInfo::kNameSize, info.name);
		out->classFlags = 0;
		copyUtf8(out->subCategories, PClassInfo2::kSubCategoriesSize, str(subCategoriesText().c_str()));
		copyUtf8(out->vendor, PClassInfo2::kVendorSize, info.vendor);
		copyUtf8(out->version, PClassInfo2::kVersionSize, info.version);
		copyUtf8(out->sdkVersion, PClassInfo2::kVersionSize, str(kVstVersionString));
		return kResultOk;
	}

	tresult PLUGIN_API getClassInfoUnicode(int32 index, PClassInfoW* out) override {
		if (out && (kindAt(index) == Kind::Controller || kindAt(index) == Kind::Compat))
			return extraClassInfo(out, kindAt(index), [&](PClassInfoW* o) {
				copyUtf16(o->name, PClassInfo::kNameSize, str(extraName(kindAt(index)).c_str()));
				o->classFlags = 0;
				copyUtf8(o->subCategories, PClassInfoW::kSubCategoriesSize, str(""));
				copyUtf16(o->vendor, PClassInfoW::kVendorSize, info.vendor);
				copyUtf16(o->version, PClassInfoW::kVersionSize, info.version);
				copyUtf16(o->sdkVersion, PClassInfoW::kVersionSize, str(kVstVersionString));
			});
		if (out && kindAt(index) == Kind::Ara)
			return araClassInfo(out, [&](PClassInfoW* o) {
				copyUtf16(o->name, PClassInfo::kNameSize, info.name);
				o->classFlags = 0;
				copyUtf8(o->subCategories, PClassInfoW::kSubCategoriesSize, str(""));
				copyUtf16(o->vendor, PClassInfoW::kVendorSize, info.vendor);
				copyUtf16(o->version, PClassInfoW::kVersionSize, info.version);
				copyUtf16(o->sdkVersion, PClassInfoW::kVersionSize, str(kVstVersionString));
			});
		if (index != 0 || !out)
			return kInvalidArgument;
		std::memcpy(out->cid, info.class_id, sizeof(TUID));
		out->cardinality = PClassInfo::kManyInstances;
		copyUtf8(out->category, PClassInfo::kCategorySize, str(kVstAudioEffectClass));
		copyUtf16(out->name, PClassInfo::kNameSize, info.name);
		out->classFlags = 0;
		copyUtf8(out->subCategories, PClassInfoW::kSubCategoriesSize, str(subCategoriesText().c_str()));
		copyUtf16(out->vendor, PClassInfoW::kVendorSize, info.vendor);
		copyUtf16(out->version, PClassInfoW::kVersionSize, info.version);
		copyUtf16(out->sdkVersion, PClassInfoW::kVersionSize, str(kVstVersionString));
		return kResultOk;
	}

	tresult PLUGIN_API createInstance(FIDString cid, FIDString requested, void** obj) override {
		if (!cid || !requested || !obj)
			return kInvalidArgument;
		*obj = nullptr;
#ifdef FERRITRON_HAVE_ARA
		TUID araId;
		araClassId(araId);
		if (araApi(api) && std::memcmp(cid, araId, sizeof(TUID)) == 0) {
			auto* factory = new AraMainFactory(araApi(api));
			tresult result = factory->queryInterface(reinterpret_cast<const int8*>(requested), obj);
			factory->release();
			return result;
		}
#endif
		if (options().compat && std::memcmp(cid, options().compatId, sizeof(TUID)) == 0) {
			auto* compat = new Compatibility(info.class_id);
			tresult result = compat->queryInterface(reinterpret_cast<const int8*>(requested), obj);
			compat->release();
			return result;
		}
		const bool asController = options().controller && std::memcmp(cid, options().controllerId, sizeof(TUID)) == 0;
		if (!asController && std::memcmp(cid, info.class_id, sizeof(TUID)) != 0)
			return kResultFalse;
		void* core = api->create();
		if (!core)
			return kResultFalse;
		if (asController) {
			// Not an instance people use: out of instances() and the MCP hub. Older cores lack it; then it is listed.
			auto* role = static_cast<const ferritron_role_api*>(api->get_extension(FERRITRON_EXT_ROLE));
			if (role && role->struct_size >= sizeof(ferritron_role_api) && role->set_role)
				role->set_role(core, FERRITRON_ROLE_CONTROLLER);
		}
		auto* plugin = new Plugin(api, core);
		plugin->separateController = asController;
		// The object starts with one reference: hand the host its own and drop ours.
		tresult result = plugin->queryInterface(reinterpret_cast<const int8*>(requested), obj);
		plugin->release();
		return result;
	}

	tresult PLUGIN_API setHostContext(FUnknown*) override { return kResultOk; }

private:
	static ferritron_str str(const char* s) { return {s, static_cast<uint32_t>(std::strlen(s))}; }

	std::string extraName(Kind kind) const {
		return std::string(info.name.ptr, info.name.len) + (kind == Kind::Controller ? " Controller" : " Compatibility");
	}

	std::string subCategoriesText() const {
		if (info.vst3_subcategories.len > 0)
			return std::string(info.vst3_subcategories.ptr, info.vst3_subcategories.len);
		return subCategories();
	}

	const char* subCategories() const {
		switch (info.kind) {
		case FERRITRON_PLUGIN_INSTRUMENT: return PlugType::kInstrumentSynth;
		case FERRITRON_PLUGIN_EFFECT:
		default: return PlugType::kFx;
		}
	}

	const ferritron_plugin_api* api;
	const ferritron_plugin_info& info;
};

} // namespace

// Called by the Rust `GetPluginFactory` export. One plugin per module.
extern "C" void* ferritron_vst3_get_factory(const ferritron_plugin_api* api) {
	if (!api || api->abi_version != FERRITRON_ABI_VERSION || api->struct_size < sizeof(ferritron_plugin_api))
		return nullptr;
	static Factory factory(api);
	return static_cast<IPluginFactory3*>(&factory);
}
