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

#include "pluginterfaces/base/ibstream.h"
#include "pluginterfaces/base/ipluginbase.h"
#include "pluginterfaces/gui/iplugview.h"
#include "pluginterfaces/gui/iplugviewcontentscalesupport.h"
#include "pluginterfaces/vst/ivstattributes.h"
#include "pluginterfaces/vst/ivstaudioprocessor.h"
#include "pluginterfaces/vst/ivstchannelcontextinfo.h"
#include "pluginterfaces/vst/ivstcomponent.h"
#include "pluginterfaces/vst/ivsteditcontroller.h"
#include "pluginterfaces/vst/ivstevents.h"
#include "pluginterfaces/vst/ivstparameterchanges.h"
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
#include <atomic>
#include <cstring>
#include <string>
#include <vector>

using namespace Steinberg;
using namespace Steinberg::Vst;

namespace {

constexpr int32 kChannels = 2; // most channels per bus: stereo, or mono when the host asks
constexpr uint32_t kMaxEvents = 1024; // automation points and notes per block

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
#ifdef FERRITRON_HAVE_ARA
                     public ARA::IPlugInEntryPoint,
                     public ARA::IPlugInEntryPoint2,
#endif
                     public ChannelContext::IInfoListener {
public:
	Plugin(const ferritron_plugin_api* api, void* core)
	: api(api), core(core), instrument(api->info->kind == FERRITRON_PLUGIN_INSTRUMENT) {
		link = static_cast<const ferritron_host_api*>(api->get_extension(FERRITRON_EXT_HOST));
		track = static_cast<const ferritron_track_api*>(api->get_extension(FERRITRON_EXT_TRACK));
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
#ifdef _WIN32
		if (host_edits::plugins++ == 0) // created on the host's main thread
			host_edits::timer = SetTimer(nullptr, 0, 30, host_edits::tick);
#else
		++host_edits::plugins;
#endif
	}

	~Plugin() {
		{
			std::lock_guard<std::mutex> guard(host_edits::lock);
			auto& q = host_edits::queue;
			q.erase(std::remove_if(q.begin(), q.end(), [this](const host_edits::Edit& e) { return e.plugin == this; }), q.end());
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
			track->set_track_info(core, &info);
		} else if (hasName) {
			link->set_track_name(core, ferritron_str{utf8, static_cast<uint32_t>(len)});
		}
		return kResultOk;
	}

	// An edit the plugin made by itself, on the main thread.
	void hostEdited(uint32_t kind, uint32_t id, double normalized) {
		if (!handler)
			return;
		if (kind == host_edits::ONE_SHOT || kind == FERRITRON_EDIT_BEGIN)
			handler->beginEdit(id);
		if (kind == host_edits::ONE_SHOT || kind == FERRITRON_EDIT_PERFORM)
			handler->performEdit(id, normalized);
		if (kind == host_edits::ONE_SHOT || kind == FERRITRON_EDIT_END)
			handler->endEdit(id);
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
		else if (sameIid(requested, IProcessContextRequirements_iid))
			*obj = static_cast<IProcessContextRequirements*>(this);
		else if (sameIid(requested, ChannelContext::IInfoListener_iid))
			*obj = static_cast<ChannelContext::IInfoListener*>(this);
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
	tresult PLUGIN_API getControllerClassId(TUID) override {
		return kResultFalse; // no separate controller: query IEditController from this object
	}

	tresult PLUGIN_API setIoMode(IoMode) override { return kResultOk; }

	// Effects: one stereo audio input and output. Instruments: one note input
	// (event bus) and one stereo audio output.
	int32 PLUGIN_API getBusCount(MediaType type, BusDirection dir) override {
		if (type == kAudio)
			return dir == kOutput || (dir == kInput && !instrument) ? 1 : 0;
		if (type == kEvent)
			return dir == kInput && instrument ? 1 : 0;
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
		bus.channelCount = type == kAudio ? busChannels : 16;
		const char* name = type == kEvent ? "Notes" : dir == kInput ? "Input" : "Output";
		copyUtf16(bus.name, 128, name, std::strlen(name));
		bus.busType = kMain;
		bus.flags = BusInfo::kDefaultActive;
		return kResultOk;
	}

	tresult PLUGIN_API getRoutingInfo(RoutingInfo&, RoutingInfo&) override { return kNotImplemented; }

	tresult PLUGIN_API activateBus(MediaType type, BusDirection dir, int32 index, TBool) override {
		return isBus(type, dir, index) ? kResultOk : kInvalidArgument;
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

	tresult PLUGIN_API setActive(TBool state) override {
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
				if (handler)
					handler->restartComponent(kLatencyChanged);
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
		std::vector<uint8_t> data(api->save_state(core, nullptr, 0));
		api->save_state(core, data.data(), static_cast<uint32_t>(data.size()));
		return writeAll(stream, data) ? kResultOk : kResultFalse;
	}

	// IAudioProcessor
	tresult PLUGIN_API setBusArrangements(SpeakerArrangement* inputs, int32 numIns, SpeakerArrangement* outputs,
	                                      int32 numOuts) override {
		// Stereo or mono; effects keep the input's layout.
		auto supported = [](SpeakerArrangement a) { return a == SpeakerArr::kStereo || a == SpeakerArr::kMono; };
		bool out = numOuts == 1 && outputs && supported(outputs[0]);
		bool in = instrument ? numIns == 0 : numIns == 1 && inputs && out && inputs[0] == outputs[0];
		if (!(in && out))
			return kResultFalse;
		busChannels = outputs[0] == SpeakerArr::kMono ? 1 : 2;
		return kResultTrue;
	}

	tresult PLUGIN_API getBusArrangement(BusDirection dir, int32 index, SpeakerArrangement& arr) override {
		if (!isBus(kAudio, dir, index))
			return kInvalidArgument;
		arr = busChannels == 1 ? SpeakerArr::kMono : SpeakerArr::kStereo;
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
		auto insert = [&](const ferritron_event& ev) {
			uint32_t at = numEvents++;
			while (at > 0 && events[at - 1].offset > ev.offset) {
				events[at] = events[at - 1];
				--at;
			}
			events[at] = ev;
		};
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
						api->set_param(core, id, value); // overflow: apply now, without timing
						continue;
					}
					ferritron_event ev{};
					ev.offset = static_cast<uint32_t>(std::max<int32>(offset, 0));
					ev.type = FERRITRON_EVENT_PARAM;
					ev.data.param.id = id;
					ev.data.param.normalized = value;
					insert(ev);
				}
			}
		}
		if (IEventList* list = instrument ? data.inputEvents : nullptr) {
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
				} else {
					continue;
				}
				insert(ev);
			}
		}
		auto flushEvents = [&] {
			for (uint32_t i = 0; i < numEvents; ++i)
				if (events[i].type == FERRITRON_EVENT_PARAM)
					api->set_param(core, events[i].data.param.id, events[i].data.param.normalized);
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
		if (!api->process(core, ptrs, static_cast<uint32_t>(channels), static_cast<uint32_t>(frames), events, numEvents,
		                  nullptr)) {
			for (int32 ch = 0; ch < channels; ++ch)
				std::memset(ptrs[ch], 0, frames * sizeof(float));
		} else {
			silent = sleepExt && sleepExt->silent(core);
		}
		// A sleeping plugin outputs silence: say so, hosts can skip work downstream.
		out.silenceFlags = silent ? (uint64(1) << channels) - 1 : 0;
		return kResultOk;
	}

	uint32 PLUGIN_API getTailSamples() override {
		if (!sleepExt)
			return kNoTail;
		uint32_t tail = sleepExt->tail(core);
		return tail == FERRITRON_TAIL_UNKNOWN ? kInfiniteTail : tail;
	}

	// IEditController
	tresult PLUGIN_API setComponentState(IBStream*) override {
		return kResultOk; // already applied through IComponent::setState on this same object
	}

	tresult setControllerStateImpl(IBStream*) override { return kResultOk; }
	tresult getControllerStateImpl(IBStream*) override { return kResultOk; }

	int32 PLUGIN_API getParameterCount() override { return static_cast<int32>(api->param_count()); }

	tresult PLUGIN_API getParameterInfo(int32 index, ParameterInfo& out) override {
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
		if (info.flags & FERRITRON_PARAM_HIDDEN)
			out.flags |= ParameterInfo::kIsHidden;
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

	ParamValue PLUGIN_API getParamNormalized(ParamID id) override { return api->get_param(core, id); }

	tresult PLUGIN_API setParamNormalized(ParamID id, ParamValue value) override {
		return api->set_param(core, id, value) ? kResultOk : kInvalidArgument;
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
		if (handler)
			handler->beginEdit(id);
	}
	void editorPerform(ParamID id, ParamValue value) {
		api->set_param(core, id, value);
		if (handler)
			handler->performEdit(id, api->get_param(core, id));
	}
	void editorEnd(ParamID id) {
		if (handler)
			handler->endEdit(id);
	}

	// Editor size in logical pixels; kept while the plugin lives, so a
	// reopened editor has the size the user chose.
	uint32_t editorWidth = 0;
	uint32_t editorHeight = 0;

	// IProcessContextRequirements
	uint32 PLUGIN_API getProcessContextRequirements() override { return 0; }

private:
	const ferritron_plugin_api* api;
	void* core;
	const ferritron_ara_api* ara = araApi(api);
	const ferritron_render_api* renderExt = static_cast<const ferritron_render_api*>(api->get_extension(FERRITRON_EXT_RENDER));
	const ferritron_sleep_api* sleepExt = static_cast<const ferritron_sleep_api*>(api->get_extension(FERRITRON_EXT_SLEEP));
	bool liveOffline = false; // last told to the plugin from process()
	int32 busChannels = kChannels;
	const bool instrument;
	std::atomic<uint32> refs{1};
	IComponentHandler* handler = nullptr;
	const ferritron_host_api* link = nullptr;
	const ferritron_track_api* track = nullptr;
	SampleRate sampleRate = 44100.0;
	int32 maxBlockSize = 1024;
	uint32 latency = 0;
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
#if defined(_WIN32) || defined(__APPLE__)
void drain(void*) {
	std::vector<Edit> pending;
	{
		std::lock_guard<std::mutex> guard(lock);
		pending.swap(queue);
	}
	for (auto& e : pending) {
		// A plugin destroyed meanwhile removed its edits from the queue under the lock.
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
	return viewApi ? new Editor(this, viewApi) : nullptr;
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

	int32 PLUGIN_API countClasses() override { return araApi(api) ? 2 : 1; }

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

	tresult PLUGIN_API getClassInfo(int32 index, PClassInfo* out) override {
		if (index == 1 && out && araApi(api))
			return araClassInfo(out, [&](PClassInfo* o) { copyUtf8(o->name, PClassInfo::kNameSize, info.name); });
		if (index != 0 || !out)
			return kInvalidArgument;
		std::memcpy(out->cid, info.class_id, sizeof(TUID));
		out->cardinality = PClassInfo::kManyInstances;
		copyUtf8(out->category, PClassInfo::kCategorySize, str(kVstAudioEffectClass));
		copyUtf8(out->name, PClassInfo::kNameSize, info.name);
		return kResultOk;
	}

	tresult PLUGIN_API getClassInfo2(int32 index, PClassInfo2* out) override {
		if (index == 1 && out && araApi(api))
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
		copyUtf8(out->subCategories, PClassInfo2::kSubCategoriesSize, str(subCategories()));
		copyUtf8(out->vendor, PClassInfo2::kVendorSize, info.vendor);
		copyUtf8(out->version, PClassInfo2::kVersionSize, info.version);
		copyUtf8(out->sdkVersion, PClassInfo2::kVersionSize, str(kVstVersionString));
		return kResultOk;
	}

	tresult PLUGIN_API getClassInfoUnicode(int32 index, PClassInfoW* out) override {
		if (index == 1 && out && araApi(api))
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
		copyUtf8(out->subCategories, PClassInfoW::kSubCategoriesSize, str(subCategories()));
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
		if (std::memcmp(cid, info.class_id, sizeof(TUID)) != 0)
			return kResultFalse;
		void* core = api->create();
		if (!core)
			return kResultFalse;
		auto* plugin = new Plugin(api, core);
		// The object starts with one reference: hand the host its own and drop ours.
		tresult result = plugin->queryInterface(reinterpret_cast<const int8*>(requested), obj);
		plugin->release();
		return result;
	}

	tresult PLUGIN_API setHostContext(FUnknown*) override { return kResultOk; }

private:
	static ferritron_str str(const char* s) { return {s, static_cast<uint32_t>(std::strlen(s))}; }

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
