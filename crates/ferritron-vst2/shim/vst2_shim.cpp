// VST 2.4 shim of Ferritron: implements the AEffect interface on top of ferritron_abi.h.
//
// Compiled against the developer's own copy of the VST 2 SDK (VST2_SDK_DIR); Ferritron does
// not ship or fetch it. The exported VSTPluginMain is defined in Rust and calls
// ferritron_vst2_main below.

// Some hosts still need the VST 2.3 idle call (audioMasterNeedIdle): keep the deprecated names.
#define VST_FORCE_DEPRECATED 0
#include "ferritron_abi.h"
#include "ferritron_events.h"
#include "pluginterfaces/vst2.x/aeffectx.h"

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#endif

extern "C" const ferritron_plugin_api* ferritron_vst2_plugin_api(void);

namespace {

constexpr uint32_t kMaxEvents = 1024;
constexpr uint32_t kMaxChannels = 32;
// Room for the plugin's output in one block: the same as the VST 3 and CLAP shims.
constexpr uint32_t kMaxOutEvents = 256;

struct Instance {
	AEffect effect; // first member: AEffect* is Instance*
	const ferritron_plugin_api* api = nullptr;
	void* core = nullptr;
	audioMasterCallback master = nullptr;
	uint32_t channels = 2;
	uint32_t scChannels = 0; // 0: no sidechain
	bool midiIn = false;
	bool instrument = false;
	// Whether the host takes MIDI from the plugin: asked once, on resume (the main thread).
	bool midiAsked = false;
	bool hostReceivesMidi = false;
	double sampleRate = 44100.0;
	int32_t blockSize = 1024;
	bool hasBypass = false;
	uint32_t bypassId = 0;
	std::vector<ferritron_param_info> params; // host-visible parameters, in order (hidden ones left out)
	ferritron_event events[kMaxEvents];
	ferritron_event sortScratch[kMaxEvents];
	uint32_t numEvents = 0;
	std::vector<uint8_t> chunk;
	std::string program = "Default";
	// The factory presets ([presets]): Ferritron's built-in Program parameter.
	uint32_t programParam = 0;
	uint32_t programCount = 0;
	const ferritron_transport_api* transportApi = nullptr;
	const ferritron_sidechain_api* sidechainApi = nullptr;
	const ferritron_layout_api* layoutApi = nullptr;
	const ferritron_main_api* mainApi = nullptr;
	const ferritron_latency_api* latencyApi = nullptr;
	// The editor (`[view]`): the plugin's web or GPU view in a child of the host's window.
	const ferritron_view_api* viewApi = nullptr;
	ferritron_view_host viewHost{};
	void* view = nullptr;
	void* editorParent = nullptr;
	double editorScale = 0; // 0: not known yet (system DPI)
	uint32_t editorWidth = 0, editorHeight = 0; // physical pixels
	ERect editorRect{};
};

Instance* self(AEffect* e) { return reinterpret_cast<Instance*>(e); }

uint32_t speakerOf(int type) {
	switch (type) {
	case kSpeakerL: return FERRITRON_SPEAKER_L;
	case kSpeakerR: return FERRITRON_SPEAKER_R;
	case kSpeakerC: case kSpeakerM: return FERRITRON_SPEAKER_C;
	case kSpeakerLfe: return FERRITRON_SPEAKER_LFE;
	case kSpeakerLs: return FERRITRON_SPEAKER_LS;
	case kSpeakerRs: return FERRITRON_SPEAKER_RS;
	case kSpeakerLc: return FERRITRON_SPEAKER_LC;
	case kSpeakerRc: return FERRITRON_SPEAKER_RC;
	case kSpeakerS: return FERRITRON_SPEAKER_CS;
	case kSpeakerSl: return FERRITRON_SPEAKER_LSS;
	case kSpeakerSr: return FERRITRON_SPEAKER_RSS;
	case kSpeakerTm: return FERRITRON_SPEAKER_TC;
	case kSpeakerTfl: return FERRITRON_SPEAKER_TFL;
	case kSpeakerTfc: return FERRITRON_SPEAKER_TFC;
	case kSpeakerTfr: return FERRITRON_SPEAKER_TFR;
	case kSpeakerTrl: return FERRITRON_SPEAKER_TRL;
	case kSpeakerTrc: return FERRITRON_SPEAKER_TRC;
	case kSpeakerTrr: return FERRITRON_SPEAKER_TRR;
	case kSpeakerLfe2: return FERRITRON_SPEAKER_LFE2;
	default: return FERRITRON_SPEAKER_OTHER + static_cast<uint32_t>(type < 0 ? -type : type);
	}
}

// Tells the plugin which speaker each channel is: the host's arrangement if it gave one, else the usual layout.
void tellLayout(Instance* in, const VstSpeakerArrangement* arr) {
	if (!in->layoutApi)
		return;
	uint32_t ids[kMaxChannels];
	const uint32_t n = std::min(in->channels, kMaxChannels);
	if (arr && static_cast<uint32_t>(arr->numChannels) == n) {
		for (uint32_t i = 0; i < n; ++i)
			ids[i] = i < 8 ? speakerOf(arr->speakers[i].type) : FERRITRON_SPEAKER_OTHER + i;
	} else {
		static const uint32_t s2[] = {FERRITRON_SPEAKER_L, FERRITRON_SPEAKER_R};
		static const uint32_t s6[] = {FERRITRON_SPEAKER_L, FERRITRON_SPEAKER_R, FERRITRON_SPEAKER_C,
		                              FERRITRON_SPEAKER_LFE, FERRITRON_SPEAKER_LS, FERRITRON_SPEAKER_RS};
		for (uint32_t i = 0; i < n; ++i)
			ids[i] = n == 1 ? FERRITRON_SPEAKER_C : n == 2 ? s2[i] : n == 6 ? s6[i] : FERRITRON_SPEAKER_OTHER + i;
	}
	in->layoutApi->set(in->core, ids, n);
}

void updateIo(Instance* in) {
	in->effect.numOutputs = static_cast<VstInt32>(in->channels);
	in->effect.numInputs = static_cast<VstInt32>((in->instrument ? 0 : in->channels) + in->scChannels);
}

void copyText(void* dst, const std::string& src, size_t cap) {
	if (!dst)
		return;
	std::strncpy(static_cast<char*>(dst), src.c_str(), cap - 1);
	static_cast<char*>(dst)[cap - 1] = 0;
}

std::string textOf(ferritron_str s) { return std::string(s.ptr ? s.ptr : "", s.len); }

// Gathered MIDI/notes for the next block (sorted once, before processing).
void addEvent(Instance* in, const ferritron_event& ev) {
	if (in->numEvents >= kMaxEvents)
		return;
	in->events[in->numEvents++] = ev;
}

void onEvents(Instance* in, const VstEvents* list) {
	if (!list || !(in->midiIn || in->instrument))
		return;
	for (VstInt32 i = 0; i < list->numEvents; ++i) {
		const VstEvent* e = list->events[i];
		if (!e || e->type != kVstMidiType)
			continue;
		const auto* m = reinterpret_cast<const VstMidiEvent*>(e);
		const uint8_t d0 = static_cast<uint8_t>(m->midiData[0]), d1 = static_cast<uint8_t>(m->midiData[1]) & 0x7F,
		              d2 = static_cast<uint8_t>(m->midiData[2]) & 0x7F;
		const uint8_t status = d0 & 0xF0, channel = d0 & 0x0F;
		ferritron_event ev{};
		ev.offset = static_cast<uint32_t>(std::max<VstInt32>(m->deltaFrames, 0));
		if (status == 0x90 || status == 0x80) {
			ev.type = status == 0x90 && d2 > 0 ? FERRITRON_EVENT_NOTE_ON : FERRITRON_EVENT_NOTE_OFF;
			ev.data.note.note_id = -1;
			ev.data.note.channel = channel;
			ev.data.note.key = d1;
			ev.data.note.velocity = static_cast<float>(d2) / 127.0f;
		} else if (status >= 0xA0 && status <= 0xE0) {
			ev.type = FERRITRON_EVENT_MIDI;
			ev.data.midi.status = status;
			ev.data.midi.channel = channel;
			ev.data.midi.data1 = d1;
			ev.data.midi.data2 = d2;
		} else {
			continue;
		}
		addEvent(in, ev);
	}
}

// The host's transport from audioMasterGetTime.
void tellTransport(Instance* in) {
	if (!in->transportApi)
		return;
	ferritron_transport t{};
	const VstIntPtr mask = kVstTempoValid | kVstTimeSigValid | kVstPpqPosValid | kVstBarsValid | kVstCyclePosValid;
	auto* info = reinterpret_cast<VstTimeInfo*>(in->master(&in->effect, audioMasterGetTime, 0, mask, nullptr, 0.0f));
	if (info) {
		if (info->flags & kVstTempoValid) {
			t.flags |= FERRITRON_TRANSPORT_TEMPO;
			t.tempo = info->tempo;
		}
		if (info->flags & kVstTimeSigValid) {
			t.flags |= FERRITRON_TRANSPORT_TIME_SIGNATURE;
			t.numerator = static_cast<uint16_t>(info->timeSigNumerator);
			t.denominator = static_cast<uint16_t>(info->timeSigDenominator);
		}
		if (info->flags & kVstTransportPlaying)
			t.flags |= FERRITRON_TRANSPORT_PLAYING;
		if (info->flags & kVstTransportRecording)
			t.flags |= FERRITRON_TRANSPORT_RECORDING;
		if (info->flags & kVstPpqPosValid) {
			t.flags |= FERRITRON_TRANSPORT_POSITION_BEATS;
			t.position_beats = info->ppqPos;
		}
		if (info->flags & kVstBarsValid) {
			t.flags |= FERRITRON_TRANSPORT_BAR_START;
			t.bar_start_beats = info->barStartPos;
		}
		t.flags |= FERRITRON_TRANSPORT_POSITION_SAMPLES;
		t.position_samples = static_cast<int64_t>(info->samplePos);
		if (info->flags & kVstCyclePosValid) {
			t.flags |= FERRITRON_TRANSPORT_LOOP_RANGE;
			t.loop_start = info->cycleStartPos;
			t.loop_end = info->cycleEndPos;
		}
		if (info->flags & kVstTransportCycleActive)
			t.flags |= FERRITRON_TRANSPORT_LOOPING;
	}
	in->transportApi->set(in->core, &t);
}

// The visible parameter with this ID, as the host knows it (its index), or -1 for a hidden one.
int editorIndexOf(Instance* in, uint32_t id) {
	for (size_t i = 0; i < in->params.size(); ++i) {
		if (in->params[i].id == id)
			return static_cast<int>(i);
	}
	return -1;
}

// A VstEvents block with room for kMaxOutEvents pointers. VstEvents declares `events[2]` as
// a variable-size array, so the host reads it through the same layout (numEvents, reserved,
// then the pointers). It lives on the stack: nothing is allocated on the audio thread.
struct OutEventList {
	VstInt32 numEvents;
	VstIntPtr reserved;
	VstEvent* events[kMaxOutEvents];
};

// What the plugin produced during the block goes to the host: parameter moves as
// audioMasterAutomate (the host records them as automation), MIDI as audioMasterProcessEvents
// (only when the host takes MIDI). Other kinds (note ends, note expression) stay in the plugin.
void sendOutEvents(Instance* in, const ferritron_event* out, uint32_t count) {
	VstMidiEvent midi[kMaxOutEvents];
	OutEventList list{};
	for (uint32_t i = 0; i < count; ++i) {
		const ferritron_event& e = out[i];
		if (e.type == FERRITRON_EVENT_PARAM) {
			if (const int index = editorIndexOf(in, e.data.param.id); index >= 0)
				in->master(&in->effect, audioMasterAutomate, index, 0, nullptr, static_cast<float>(e.data.param.normalized));
		} else if (e.type == FERRITRON_EVENT_MIDI && in->hostReceivesMidi && list.numEvents < kMaxOutEvents) {
			VstMidiEvent& m = midi[list.numEvents];
			std::memset(&m, 0, sizeof(m));
			m.type = kVstMidiType;
			m.byteSize = static_cast<VstInt32>(sizeof(VstMidiEvent));
			m.deltaFrames = static_cast<VstInt32>(e.offset);
			m.midiData[0] = static_cast<char>((e.data.midi.status & 0xF0) | (e.data.midi.channel & 0x0F));
			m.midiData[1] = static_cast<char>(e.data.midi.data1 & 0x7F);
			m.midiData[2] = static_cast<char>(e.data.midi.data2 & 0x7F);
			list.events[list.numEvents++] = reinterpret_cast<VstEvent*>(&m);
		}
	}
	if (list.numEvents > 0)
		in->master(&in->effect, audioMasterProcessEvents, 0, 0, &list, 0.0f);
}

void processReplacing(AEffect* e, float** inputs, float** outputs, VstInt32 frames) {
	Instance* in = self(e);
	const uint32_t channels = std::min(in->channels, kMaxChannels);
	const size_t bytes = static_cast<size_t>(std::max<VstInt32>(frames, 0)) * sizeof(float);
	const uint32_t mainIn = in->instrument ? 0 : channels;
	float* ptrs[kMaxChannels] = {};
	for (uint32_t ch = 0; ch < channels; ++ch) {
		float* dst = outputs[ch];
		const float* src = ch < mainIn && inputs ? inputs[ch] : nullptr;
		if (!src)
			std::memset(dst, 0, bytes);
		else if (src != dst)
			std::memmove(dst, src, bytes);
		ptrs[ch] = dst;
	}
	if (in->sidechainApi) {
		if (in->scChannels > 0 && inputs)
			in->sidechainApi->set(in->core, const_cast<const float* const*>(inputs + mainIn), in->scChannels);
		else
			in->sidechainApi->set(in->core, nullptr, 0);
	}
	tellTransport(in);
	ferritron_sort_events(in->events, in->sortScratch, in->numEvents);
	ferritron_event out[kMaxOutEvents];
	uint32_t outCount = 0;
	const ferritron_process_data pd = {sizeof(pd), static_cast<uint32_t>(frames), channels, in->numEvents, ptrs, in->events, nullptr, out, kMaxOutEvents, &outCount};
	if (!in->api->process(in->core, &pd)) {
		for (uint32_t ch = 0; ch < channels; ++ch)
			std::memset(ptrs[ch], 0, bytes);
	} else {
		sendOutEvents(in, out, std::min(outCount, kMaxOutEvents));
	}
	in->numEvents = 0;
	// The plugin asked something of the host's main thread (a new latency, values changed): ask for an idle call.
	if (in->mainApi && in->mainApi->pending(in->core))
		in->master(e, audioMasterNeedIdle, 0, 0, nullptr, 0.0f);
}

void processDoubleReplacing(AEffect*, double**, double**, VstInt32) {}

void setParameter(AEffect* e, VstInt32 index, float value) {
	Instance* in = self(e);
	if (index >= 0 && static_cast<size_t>(index) < in->params.size())
		in->api->set_param(in->core, in->params[static_cast<size_t>(index)].id, value);
}

float getParameter(AEffect* e, VstInt32 index) {
	Instance* in = self(e);
	if (index < 0 || static_cast<size_t>(index) >= in->params.size())
		return 0.0f;
	return static_cast<float>(in->api->get_param(in->core, in->params[static_cast<size_t>(index)].id));
}

// Main-thread work the plugin asked for (called from the host's idle calls).
void poll(Instance* in) {
	if (!in->mainApi)
		return;
	const uint32_t bits = in->mainApi->take_requests(in->core);
	if (bits & FERRITRON_REQUEST_LATENCY) {
		in->effect.initialDelay = in->latencyApi ? static_cast<VstInt32>(in->latencyApi->get(in->core)) : 0;
		in->master(&in->effect, audioMasterIOChanged, 0, 0, nullptr, 0.0f);
	}
	if (bits & FERRITRON_REQUEST_VALUES)
		in->master(&in->effect, audioMasterUpdateDisplay, 0, 0, nullptr, 0.0f);
}

int32_t categoryOf(Instance* in) {
	if (in->instrument)
		return kPlugCategSynth;
	const std::string sub = textOf(in->api->info->vst3_subcategories);
	auto has = [&](const char* word) { return sub.find(word) != std::string::npos; };
	if (has("Analyzer"))
		return kPlugCategAnalysis;
	if (has("Mastering") || has("Dynamics"))
		return kPlugCategMastering;
	if (has("Spatial"))
		return kPlugCategSpacializer;
	if (has("Reverb") || has("Delay"))
		return kPlugCategRoomFx;
	if (has("Restoration"))
		return kPlugCategRestoration;
	if (has("Generator"))
		return kPlugCategGenerator;
	return kPlugCategEffect;
}

// --- Editor (effEditGetRect / effEditOpen / effEditClose) ----------------------
//
// VST 2 sizes are physical pixels. The view's own sizes are logical, so the window is the logical
// size times the scale: the DPI of the host's window once it is open, else the system's.

double editorScaleFor(Instance* in) {
#ifdef _WIN32
	using GetDpiForWindowFn = UINT(WINAPI*)(HWND);
	using GetDpiForSystemFn = UINT(WINAPI*)();
	static const auto forWindow = reinterpret_cast<GetDpiForWindowFn>(GetProcAddress(GetModuleHandleA("user32.dll"), "GetDpiForWindow"));
	static const auto forSystem = reinterpret_cast<GetDpiForSystemFn>(GetProcAddress(GetModuleHandleA("user32.dll"), "GetDpiForSystem"));
	UINT dpi = 0;
	if (in->editorParent && forWindow)
		dpi = forWindow(static_cast<HWND>(in->editorParent));
	if (!dpi && forSystem)
		dpi = forSystem();
	return dpi ? dpi / 96.0 : 1.0;
#else
	(void)in;
	return 1.0;
#endif
}

void editorSetRect(Instance* in) {
	const double scale = editorScaleFor(in);
	if (in->editorWidth == 0) {
		in->editorWidth = static_cast<uint32_t>(std::lround(in->viewApi->width * scale));
		in->editorHeight = static_cast<uint32_t>(std::lround(in->viewApi->height * scale));
	}
	in->editorRect.left = 0;
	in->editorRect.top = 0;
	in->editorRect.right = static_cast<short>(std::min<uint32_t>(in->editorWidth, 32767));
	in->editorRect.bottom = static_cast<short>(std::min<uint32_t>(in->editorHeight, 32767));
}

void editorOpenCallbacks(Instance* in) {
	ferritron_view_host& host = in->viewHost;
	host.ctx = in;
	host.get_param = [](void* ctx, uint32_t id) { return static_cast<Instance*>(ctx)->api->get_param(static_cast<Instance*>(ctx)->core, id); };
	host.begin_edit = [](void* ctx, uint32_t id) {
		auto* in = static_cast<Instance*>(ctx);
		if (int i = editorIndexOf(in, id); i >= 0)
			in->master(&in->effect, audioMasterBeginEdit, i, 0, nullptr, 0.0f);
	};
	host.perform_edit = [](void* ctx, uint32_t id, double normalized) {
		auto* in = static_cast<Instance*>(ctx);
		in->api->set_param(in->core, id, normalized);
		if (int i = editorIndexOf(in, id); i >= 0)
			in->master(&in->effect, audioMasterAutomate, i, 0, nullptr, static_cast<float>(normalized));
	};
	host.end_edit = [](void* ctx, uint32_t id) {
		auto* in = static_cast<Instance*>(ctx);
		if (int i = editorIndexOf(in, id); i >= 0)
			in->master(&in->effect, audioMasterEndEdit, i, 0, nullptr, 0.0f);
	};
	// audioMasterSizeWindow: the host answers non-zero when it resized the window.
	host.request_resize = [](void* ctx, uint32_t w, uint32_t h) -> bool {
		auto* in = static_cast<Instance*>(ctx);
		if (!in->view || !in->master(&in->effect, audioMasterSizeWindow, static_cast<VstInt32>(w), static_cast<VstIntPtr>(h), nullptr, 0.0f))
			return false;
		in->editorWidth = w;
		in->editorHeight = h;
		editorSetRect(in);
		in->viewApi->set_size(in->view, w, h, editorScaleFor(in));
		return true;
	};
}

bool editorOpen(Instance* in, void* parent) {
	if (!in->viewApi || !parent || in->view)
		return false;
	in->editorParent = parent;
	in->editorWidth = in->editorHeight = 0; // recomputed with the window's own DPI
	editorSetRect(in);
	editorOpenCallbacks(in);
	in->view = in->viewApi->open(parent, &in->viewHost, in->editorWidth, in->editorHeight, editorScaleFor(in));
	if (!in->view)
		in->editorParent = nullptr;
	return in->view != nullptr;
}

void editorClose(Instance* in) {
	if (!in->view)
		return;
	in->viewApi->close(in->view);
	in->view = nullptr;
	in->editorParent = nullptr;
}

VstIntPtr dispatcher(AEffect* e, VstInt32 opcode, VstInt32 index, VstIntPtr value, void* ptr, float opt) {
	Instance* in = self(e);
	switch (opcode) {
	case effOpen:
		tellLayout(in, nullptr);
		return 0;
	case effClose:
		editorClose(in);
		in->api->destroy(in->core);
		delete in;
		return 0;
	case effSetProgram:
		// A factory preset: set the Program parameter; the main thread loads it (now, if this is it).
		if (in->programCount > 0 && value >= 0 && static_cast<uint32_t>(value) < in->programCount) {
			in->api->set_param(in->core, in->programParam, in->api->to_normalized(in->programParam, static_cast<double>(value)));
			poll(in);
		}
		return 0;
	case effGetProgram:
		if (in->programCount == 0)
			return 0;
		return static_cast<VstIntPtr>(std::lround(in->api->to_plain(in->programParam, in->api->get_param(in->core, in->programParam))));
	case effGetProgramName:
	case effGetProgramNameIndexed: {
		if (in->programCount == 0) {
			copyText(ptr, in->program, kVstMaxProgNameLen);
			return 1;
		}
		double which = opcode == effGetProgramNameIndexed ? static_cast<double>(index) : in->api->to_plain(in->programParam, in->api->get_param(in->core, in->programParam));
		if (which < 0 || which >= in->programCount)
			return 0;
		char buf[256];
		int32_t len = in->api->format_param(in->programParam, in->api->to_normalized(in->programParam, which), buf, sizeof(buf));
		copyText(ptr, std::string(buf, static_cast<size_t>(std::max(len, 0))), kVstMaxProgNameLen);
		return 1;
	}
	case effSetProgramName:
		if (ptr && in->programCount == 0)
			in->program = static_cast<const char*>(ptr);
		return 0;
	case effGetParamLabel:
		if (index >= 0 && static_cast<size_t>(index) < in->params.size())
			copyText(ptr, textOf(in->params[static_cast<size_t>(index)].unit), 32);
		return 0;
	case effGetParamDisplay: {
		if (index < 0 || static_cast<size_t>(index) >= in->params.size())
			return 0;
		const auto& p = in->params[static_cast<size_t>(index)];
		char buf[256];
		const int32_t len = in->api->format_param(p.id, in->api->get_param(in->core, p.id), buf, sizeof(buf));
		buf[std::clamp<int32_t>(len, 0, sizeof(buf) - 1)] = 0;
		copyText(ptr, buf, 64);
		return 0;
	}
	case effGetParamName:
		if (index >= 0 && static_cast<size_t>(index) < in->params.size())
			copyText(ptr, textOf(in->params[static_cast<size_t>(index)].name), 32);
		return 0;
	case effString2Parameter: {
		if (index < 0 || static_cast<size_t>(index) >= in->params.size() || !ptr)
			return 0;
		const auto& p = in->params[static_cast<size_t>(index)];
		const char* text = static_cast<const char*>(ptr);
		ferritron_str s{text, static_cast<uint32_t>(std::strlen(text))};
		double normalized = 0.0;
		if (!in->api->parse_param(p.id, s, &normalized))
			return 0;
		in->api->set_param(in->core, p.id, normalized);
		return 1;
	}
	case effCanBeAutomated:
		return index >= 0 && static_cast<size_t>(index) < in->params.size() &&
		               !(in->params[static_cast<size_t>(index)].flags & FERRITRON_PARAM_NOT_AUTOMATABLE)
		           ? 1
		           : 0;
	case effGetParameterProperties:
		return 0;
	case effSetSampleRate:
		in->sampleRate = opt;
		return 0;
	case effSetBlockSize:
		in->blockSize = static_cast<int32_t>(value);
		return 0;
	case effMainsChanged:
		if (value) {
			// Asked once, here on the main thread: does the host take MIDI from the plugin?
			if (!in->midiAsked) {
				in->midiAsked = true;
				in->hostReceivesMidi = in->master(&in->effect, audioMasterCanDo, 0, 0, const_cast<char*>("receiveVstMidiEvent"), 0.0f) > 0;
			}
			tellLayout(in, nullptr);
			in->api->activate(in->core, in->sampleRate, static_cast<uint32_t>(std::max(in->blockSize, 1)));
			in->effect.initialDelay = in->latencyApi ? static_cast<VstInt32>(in->latencyApi->get(in->core)) : 0;
		}
		return 0;
	case effEditGetRect:
		if (!in->viewApi || !ptr)
			return 0;
		editorSetRect(in);
		*static_cast<ERect**>(ptr) = &in->editorRect;
		return 1;
	case effEditOpen:
		return editorOpen(in, ptr) ? 1 : 0;
	case effEditClose:
		editorClose(in);
		return 0;
	case effEditIdle:
		poll(in);
		return 0;
	case effGetChunk: {
		if (!ptr)
			return 0;
		// The size can grow between the size query and the copy: try again, never save zeros.
		for (int attempt = 0; attempt < 3; ++attempt) {
			const uint32_t size = in->api->save_state(in->core, nullptr, 0);
			in->chunk.assign(size, 0);
			const uint32_t written = in->api->save_state(in->core, in->chunk.data(), size);
			if (written <= size) {
				in->chunk.resize(written);
				*static_cast<void**>(ptr) = in->chunk.data();
				return static_cast<VstIntPtr>(written);
			}
		}
		return 0;
	}
	case effSetChunk:
		return ptr && in->api->load_state(in->core, static_cast<const uint8_t*>(ptr), static_cast<uint32_t>(value)) ? 1 : 0;
	case effProcessEvents:
		onEvents(in, static_cast<const VstEvents*>(ptr));
		return 1;
	case effGetPlugCategory:
		return categoryOf(in);
	case effGetEffectName:
	case effGetProductString:
		copyText(ptr, textOf(in->api->info->name), 64);
		return 1;
	case effGetVendorString:
		copyText(ptr, textOf(in->api->info->vendor), 64);
		return 1;
	case effGetVendorVersion:
		return in->effect.version;
	case effGetVstVersion:
		return 2400;
	case effGetTailSize:
		return 0;
	case effSetBypass:
		if (in->hasBypass)
			in->api->set_param(in->core, in->bypassId, value ? 1.0 : 0.0);
		return in->hasBypass ? 1 : 0;
	case effSetSpeakerArrangement: {
		const auto* out = static_cast<const VstSpeakerArrangement*>(ptr);
		if (!out || out->numChannels < static_cast<VstInt32>(in->api->info->min_channels) ||
		    out->numChannels > static_cast<VstInt32>(in->api->info->max_channels) || out->numChannels > static_cast<VstInt32>(kMaxChannels))
			return 0;
		in->channels = static_cast<uint32_t>(out->numChannels);
		updateIo(in);
		tellLayout(in, out);
		return 1;
	}
	case effGetSpeakerArrangement: {
		auto* inArr = reinterpret_cast<VstSpeakerArrangement**>(value);
		auto* outArr = static_cast<VstSpeakerArrangement**>(ptr);
		(void)inArr;
		(void)outArr;
		return 0; // the host reads numInputs / numOutputs
	}
	case effCanDo: {
		const char* what = static_cast<const char*>(ptr);
		if (!what)
			return 0;
		auto is = [&](const char* s) { return std::strcmp(what, s) == 0; };
		if (is("receiveVstEvents") || is("receiveVstMidiEvent"))
			return in->midiIn || in->instrument ? 1 : -1;
		if (is("receiveVstTimeInfo") || is("bypass"))
			return 1;
		// The plugin sends MIDI and parameter moves (sendVstMidiEvent / sendVstEvents): the
		// format has no manifest flag for output, so every plugin says yes; the shim sends only
		// what the plugin wrote, and nothing when the host does not take MIDI.
		if (is("sendVstEvents") || is("sendVstMidiEvent"))
			return 1;
		return 0;
	}
	default:
		return 0;
	}
}

} // namespace

extern "C" AEffect* ferritron_vst2_main(audioMasterCallback master) {
	const ferritron_plugin_api* api = ferritron_vst2_plugin_api();
	if (!master || !api)
		return nullptr;
	// A host that does not answer is not a VST 2.4 host.
	if (master(nullptr, audioMasterVersion, 0, 0, nullptr, 0.0f) == 0)
		return nullptr;
	void* core = api->create();
	if (!core)
		return nullptr;
	auto* in = new Instance();
	in->api = api;
	in->core = core;
	in->master = master;
	const ferritron_plugin_info& info = *api->info;
	in->instrument = info.kind == FERRITRON_PLUGIN_INSTRUMENT;
	in->midiIn = (info.audio_flags & FERRITRON_AUDIO_MIDI_INPUT) != 0;
	in->transportApi = static_cast<const ferritron_transport_api*>(api->get_extension(FERRITRON_EXT_TRANSPORT));
	in->sidechainApi = static_cast<const ferritron_sidechain_api*>(api->get_extension(FERRITRON_EXT_SIDECHAIN));
	in->layoutApi = static_cast<const ferritron_layout_api*>(api->get_extension(FERRITRON_EXT_LAYOUT));
	in->mainApi = static_cast<const ferritron_main_api*>(api->get_extension(FERRITRON_EXT_MAIN));
	in->latencyApi = static_cast<const ferritron_latency_api*>(api->get_extension(FERRITRON_EXT_LATENCY));
	in->viewApi = static_cast<const ferritron_view_api*>(api->get_extension(FERRITRON_EXT_VIEW));
	uint32_t channels = std::clamp<uint32_t>(2, std::max(info.min_channels, 1u), std::max(info.max_channels, std::max(info.min_channels, 1u)));
	in->channels = std::min(channels, kMaxChannels);
	if ((info.audio_flags & FERRITRON_AUDIO_SIDECHAIN) && !in->instrument && in->sidechainApi)
		in->scChannels = info.sidechain_channels ? info.sidechain_channels : in->channels;
	for (uint32_t i = 0, n = api->param_count(); i < n; ++i) {
		ferritron_param_info p{};
		if (!api->param_info(i, &p))
			continue;
		if (p.flags & FERRITRON_PARAM_BYPASS) {
			in->hasBypass = true;
			in->bypassId = p.id;
		}
		if (p.flags & FERRITRON_PARAM_PROGRAM) {
			in->programParam = p.id;
			in->programCount = p.step_count + 1;
		}
		if (p.flags & FERRITRON_PARAM_HIDDEN)
			continue;
		in->params.push_back(p);
	}

	AEffect& e = in->effect;
	std::memset(&e, 0, sizeof(e));
	e.magic = kEffectMagic;
	e.dispatcher = dispatcher;
	e.process = processReplacing; // the deprecated accumulating call is served the same way
	e.processReplacing = processReplacing;
	e.processDoubleReplacing = processDoubleReplacing;
	e.setParameter = setParameter;
	e.getParameter = getParameter;
	e.numPrograms = in->programCount > 0 ? static_cast<VstInt32>(in->programCount) : 1;
	e.numParams = static_cast<VstInt32>(in->params.size());
	e.flags = effFlagsCanReplacing | effFlagsProgramChunks | (in->instrument ? effFlagsIsSynth : 0) | (in->viewApi ? effFlagsHasEditor : 0);
	e.object = in;
	e.version = 1;
	if (info.vst2_id) {
		e.uniqueID = static_cast<VstInt32>(info.vst2_id);
	} else {
		uint32_t h = 2166136261u;
		for (unsigned char b : info.class_id)
			h = (h ^ b) * 16777619u;
		e.uniqueID = static_cast<VstInt32>(h);
	}
	updateIo(in);
	return &e;
}
