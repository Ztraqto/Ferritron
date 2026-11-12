// AAX (Native) on top of ferritron_abi.h, compiled against the developer's
// own AAX SDK (AAX_SDK_DIR). Built on the SDK's AAX_CMonolithicParameters:
// one object holds the parameters, the state and the plugin instance, and
// renders audio (with MIDI for instruments) in its RenderAudio.
//
// - Parameters: one AAX parameter per Ferritron parameter, normalized
//   0..1, shown and typed with the plugin's own text (format/parse). The
//   bypass parameter is Pro Tools' Master Bypass.
// - State: the plugin's state in one chunk ('Frtn').
// - Editor: the plugin's editor (web or GPU) in the HWND / NSView Pro Tools
//   gives the GUI object.
// - Latency: reported with SetSignalLatency after activation.
//
// 1.3.1: compiled and linked against the AAX SDK (AAX_SDK_DIR), but never run in Pro
// Tools (no validator or Pro Tools here). Pro Tools loads only PACE-signed plugins
// (wraptool) except in Pro Tools Developer.
//
// - MIDI: all messages (CC, pitch bend, pressure, program change) for instruments and
//   for effects with `midi_input`.
// - Transport: tempo, meter, play state, position and loop from the transport node.
// - Channels: one effect registration per stem format in the plugin's channel range
//   (stereo only for the default 1..2 range, as in 1.3.0).
// - Latency changes and bulk value changes: applied from the host's timer (TimerWakeup).
// - Parameter changes the plugin makes itself (FERRITRON_EVENT_PARAM): queued on the render
//   thread and applied from the same timer.
// - MIDI out (FERRITRON_EVENT_MIDI): posted to an output MIDI node (AAX_eMIDINodeType_LocalOutput)
//   on the second slot of mAdditionalInputMIDINodes, on every instrument and effect (no flag says
//   which send MIDI). Compiled against the SDK; not run in Pro Tools.
// - Sidechain: AAX has one mono sidechain input. AAX_CMonolithicParameters::StaticDescribe
//   has no field for it and no field for a MIDI output, so DescribeWithNodes() below repeats its
//   work and registers the sidechain on the first slot of mAdditionalInputMIDINodes and the MIDI
//   output on the second; the plugin reads the sidechain as left = right.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

#include "ferritron_abi.h"
#include "ferritron_events.h"

#include "AAX_CEffectGUI.h"
#include "AAX_CMonolithicParameters.h"
#include "AAX_CLinearTaperDelegate.h"
#include "AAX_CParameter.h"
#include "AAX_ICollection.h"
#include "AAX_IComponentDescriptor.h"
#include "AAX_IEffectDescriptor.h"
#include "AAX_IMIDINode.h"
#include "AAX_ITransport.h"
#include "AAX_Properties.h"
#include "AAX_IPropertyMap.h"
#include "AAX_IViewContainer.h"
#include "AAX_IController.h"

#include <algorithm>
#include <atomic>
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

extern "C" const ferritron_plugin_api* ferritron_aax_plugin_api(void);

namespace {

const ferritron_plugin_api* api() { return ferritron_aax_plugin_api(); }

std::string str(ferritron_str s) { return s.ptr ? std::string(s.ptr, s.len) : std::string(); }

// Stable four-character codes from text (FNV-1a), printable.
AAX_CTypeID code_of(const std::string& text, uint64_t seed) {
    uint64_t h = 0xcbf29ce484222325ull ^ seed;
    for (unsigned char c : text) h = (h ^ c) * 0x100000001b3ull;
    static const char alphabet[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    AAX_CTypeID out = 0;
    for (int i = 0; i < 4; i++) out = (out << 8) | (unsigned char)alphabet[(h >> (i * 8)) % 62];
    return out;
}

// Parameter ids: "p" + the stable Ferritron id; the bypass is Pro Tools'.
std::string param_key(const ferritron_param_info& info) {
    if (info.flags & FERRITRON_PARAM_BYPASS) return cDefaultMasterBypassID;
    char key[16];
    std::snprintf(key, sizeof key, "p%08x", info.id);
    return key;
}

std::vector<ferritron_param_info> params() {
    std::vector<ferritron_param_info> out;
    for (uint32_t i = 0; i < api()->param_count(); i++) {
        ferritron_param_info info{};
        if (api()->param_info(i, &info)) out.push_back(info);
    }
    return out;
}

// Display text from the plugin (dB, choice names, ...), typed values back.
class TextDelegate : public AAX_IDisplayDelegate<double> {
public:
    explicit TextDelegate(uint32_t id) : id_(id) {}
    AAX_IDisplayDelegate<double>* Clone() const override { return new TextDelegate(*this); }
    bool ValueToString(double value, AAX_CString* out) const override {
        char buf[128];
        int32_t n = api()->format_param(id_, value, buf, sizeof buf);
        if (n < 0) return false;
        *out = AAX_CString(std::string(buf, (size_t)n).c_str());
        return true;
    }
    bool ValueToString(double value, int32_t, AAX_CString* out) const override { return ValueToString(value, out); }
    bool StringToValue(const AAX_CString& text, double* value) const override {
        ferritron_str s{text.Get(), (uint32_t)std::strlen(text.Get())};
        return api()->parse_param(id_, s, value);
    }

private:
    uint32_t id_;
};

const AAX_CTypeID kChunkID = 'Frtn';

// Slots of AAX_SInstrumentRenderInfo::mAdditionalInputMIDINodes this shim uses: the sidechain
// (slot 0, a buffer address) and the MIDI output node (kMidiOutSlot), see DescribeWithNodes.
const int32_t kMidiOutSlot = 1;

// The stem formats Ferritron offers, with their channel counts and speakers (FERRITRON_SPEAKER_*).
struct Stem {
    AAX_EStemFormat format;
    uint32_t channels;
};
const Stem kStems[] = {
    {AAX_eStemFormat_Mono, 1},     {AAX_eStemFormat_Stereo, 2},    {AAX_eStemFormat_LCR, 3},      {AAX_eStemFormat_Quad, 4},
    {AAX_eStemFormat_5_0, 5},      {AAX_eStemFormat_5_1, 6},       {AAX_eStemFormat_7_1_DTS, 8},  {AAX_eStemFormat_7_1_2, 10},
    {AAX_eStemFormat_7_1_4, 12},   {AAX_eStemFormat_7_1_6, 14},    {AAX_eStemFormat_9_1_6, 16},
};

std::vector<uint32_t> speakers_of(AAX_EStemFormat f, uint32_t channels) {
    const uint32_t L = FERRITRON_SPEAKER_L, R = FERRITRON_SPEAKER_R, C = FERRITRON_SPEAKER_C, LFE = FERRITRON_SPEAKER_LFE,
                   Ls = FERRITRON_SPEAKER_LS, Rs = FERRITRON_SPEAKER_RS, Lss = FERRITRON_SPEAKER_LSS, Rss = FERRITRON_SPEAKER_RSS,
                   Lsr = FERRITRON_SPEAKER_LSR, Rsr = FERRITRON_SPEAKER_RSR, Tfl = FERRITRON_SPEAKER_TFL, Tfr = FERRITRON_SPEAKER_TFR,
                   Trl = FERRITRON_SPEAKER_TRL, Trr = FERRITRON_SPEAKER_TRR, Tsl = FERRITRON_SPEAKER_TSL, Tsr = FERRITRON_SPEAKER_TSR,
                   Lw = FERRITRON_SPEAKER_LW, Rw = FERRITRON_SPEAKER_RW;
    std::vector<uint32_t> out;
    switch (f) {
    case AAX_eStemFormat_Mono: out = {C}; break;
    case AAX_eStemFormat_Stereo: out = {L, R}; break;
    case AAX_eStemFormat_LCR: out = {L, C, R}; break;
    case AAX_eStemFormat_Quad: out = {L, R, Ls, Rs}; break;
    case AAX_eStemFormat_5_0: out = {L, C, R, Ls, Rs}; break;
    case AAX_eStemFormat_5_1: out = {L, C, R, Ls, Rs, LFE}; break;
    case AAX_eStemFormat_7_1_DTS: out = {L, C, R, Lss, Rss, Lsr, Rsr, LFE}; break;
    case AAX_eStemFormat_7_1_2: out = {L, C, R, Lss, Rss, Lsr, Rsr, LFE, Tsl, Tsr}; break;
    case AAX_eStemFormat_7_1_4: out = {L, C, R, Lss, Rss, Lsr, Rsr, LFE, Tfl, Tfr, Trl, Trr}; break;
    case AAX_eStemFormat_7_1_6: out = {L, C, R, Lss, Rss, Lsr, Rsr, LFE, Tfl, Tfr, Tsl, Tsr, Trl, Trr}; break;
    case AAX_eStemFormat_9_1_6: out = {L, C, R, Lw, Rw, Lss, Rss, Lsr, Rsr, LFE, Tfl, Tfr, Tsl, Tsr, Trl, Trr}; break;
    default: break;
    }
    // Any other format: positions without names.
    while (out.size() < channels) out.push_back(FERRITRON_SPEAKER_OTHER + (uint32_t)out.size());
    out.resize(channels);
    return out;
}

class Parameters;

// The plugin's editor inside Pro Tools' view container.
class Gui : public AAX_CEffectGUI {
public:
    static AAX_IEffectGUI* AAX_CALLBACK Create() { return new Gui(); }

protected:
    void CreateViewContents() override {}

    void CreateViewContainer() override {
        auto* view = static_cast<const ferritron_view_api*>(api()->get_extension(FERRITRON_EXT_VIEW));
        void* parent = GetViewContainerPtr();
        if (!view || !parent) return;
        host_ = ferritron_view_host{GetEffectParameters(), &Gui::get, &Gui::begin, &Gui::perform, &Gui::end, nullptr};
        view_api_ = view;
        // Pro Tools hands logical sizes; the editor scales itself.
        view_ = view->open(parent, &host_, view->width, view->height, 1.0);
    }

    void DeleteViewContainer() override {
        if (view_ && view_api_) view_api_->close(view_);
        view_ = nullptr;
    }

    AAX_Result GetViewSize(AAX_Point* size) const override {
        auto* view = static_cast<const ferritron_view_api*>(api()->get_extension(FERRITRON_EXT_VIEW));
        if (!view) return AAX_ERROR_NULL_OBJECT;
        size->horz = (float)view->width;
        size->vert = (float)view->height;
        return AAX_SUCCESS;
    }

private:
    static AAX_IEffectParameters* params_of(void* ctx) { return static_cast<AAX_IEffectParameters*>(ctx); }
    static std::string key_of(uint32_t id) {
        for (auto& info : params())
            if (info.id == id) return param_key(info);
        return {};
    }
    static double get(void* ctx, uint32_t id) {
        double value = 0;
        params_of(ctx)->GetParameterNormalizedValue(key_of(id).c_str(), &value);
        return value;
    }
    static void begin(void* ctx, uint32_t id) { params_of(ctx)->TouchParameter(key_of(id).c_str()); }
    static void perform(void* ctx, uint32_t id, double value) { params_of(ctx)->SetParameterNormalizedValue(key_of(id).c_str(), value); }
    static void end(void* ctx, uint32_t id) { params_of(ctx)->ReleaseParameter(key_of(id).c_str()); }

    ferritron_view_host host_{};
    const ferritron_view_api* view_api_ = nullptr;
    void* view_ = nullptr;
};

class Parameters : public AAX_CMonolithicParameters {
public:
    static AAX_CEffectParameters* AAX_CALLBACK Create() { return new Parameters(); }

    ~Parameters() override {
        if (instance_) api()->destroy(instance_);
    }

    AAX_Result EffectInit() override {
        instance_ = api()->create();
        if (!instance_) return AAX_ERROR_NULL_OBJECT;
        // The editor finds its instance through the view host's ctx: the
        // GUI passes this object (GetEffectParameters()).
        if (auto* host2 = static_cast<const ferritron_host_api2*>(api()->get_extension(FERRITRON_EXT_HOST2))) {
            ferritron_host_callbacks2 callbacks{static_cast<AAX_IEffectParameters*>(this), nullptr, nullptr};
            host2->attach(instance_, &callbacks);
        }
        for (auto& info : params()) {
            std::string key = param_key(info);
            ids_.push_back({key, info.id});
            if (key == cDefaultMasterBypassID) {
                // AAX_CMonolithicParameters made the Master Bypass already.
                continue;
            }
            auto* p = new AAX_CParameter<double>(key.c_str(), AAX_CString(str(info.name).c_str()), info.default_normalized,
                                                 AAX_CLinearTaperDelegate<double>(0.0, 1.0), TextDelegate(info.id),
                                                 !(info.flags & FERRITRON_PARAM_NOT_AUTOMATABLE));
            if (info.step_count > 0) p->SetNumberOfSteps(info.step_count + 1);
            p->SetType(info.step_count > 0 ? AAX_eParameterType_Discrete : AAX_eParameterType_Continuous);
            mParameterManager.AddParameter(p);
            AddSynchronizedParameter(*p);
        }
        // Room for one change of every parameter plus kMaxMidi MIDI messages per block:
        // the render thread never grows these (it drops what does not fit).
        maxEvents_ = ids_.size() + kMaxMidi;
        events_.reserve(maxEvents_);
        sortScratch_.assign(maxEvents_, ferritron_event{});
        AAX_CSampleRate rate = 48000;
        Controller()->GetSampleRate(&rate);
        transport_ = static_cast<const ferritron_transport_api*>(api()->get_extension(FERRITRON_EXT_TRANSPORT));
        main_ = static_cast<const ferritron_main_api*>(api()->get_extension(FERRITRON_EXT_MAIN));
        if (api()->info->audio_flags & FERRITRON_AUDIO_SIDECHAIN)
            sidechain_ = static_cast<const ferritron_sidechain_api*>(api()->get_extension(FERRITRON_EXT_SIDECHAIN));
        AAX_EStemFormat stem = AAX_eStemFormat_Stereo;
        Controller()->GetOutputStemFormat(&stem);
        channels_ = std::min<uint32_t>(AAX_STEM_FORMAT_CHANNEL_COUNT(stem), 32);
        if (auto* layout = static_cast<const ferritron_layout_api*>(api()->get_extension(FERRITRON_EXT_LAYOUT))) {
            std::vector<uint32_t> ids = speakers_of(stem, channels_);
            layout->set(instance_, ids.data(), (uint32_t)ids.size());
        }
        api()->activate(instance_, rate, kMaxBlock);
        if (auto* latency = static_cast<const ferritron_latency_api*>(api()->get_extension(FERRITRON_EXT_LATENCY)))
            Controller()->SetSignalLatency((int32_t)latency->get(instance_));
        return AAX_SUCCESS;
    }

    // The host's timer, on the main thread: a new latency and values set in bulk.
    AAX_Result TimerWakeup() override {
        ApplyEdits();
        if (main_ && instance_) {
            const uint32_t bits = main_->take_requests(instance_);
            if (bits & FERRITRON_REQUEST_LATENCY) {
                if (auto* latency = static_cast<const ferritron_latency_api*>(api()->get_extension(FERRITRON_EXT_LATENCY)))
                    Controller()->SetSignalLatency((int32_t)latency->get(instance_));
            }
            if (bits & FERRITRON_REQUEST_VALUES)
                for (auto& [key, pid] : ids_) SetParameterNormalizedValue(key.c_str(), api()->get_param(instance_, pid));
        }
        return AAX_CMonolithicParameters::TimerWakeup();
    }

    // State: the plugin's own, in one chunk.
    AAX_Result GetNumberOfChunks(int32_t* count) const override {
        *count = 1;
        return AAX_SUCCESS;
    }
    AAX_Result GetChunkIDFromIndex(int32_t index, AAX_CTypeID* id) const override {
        if (index != 0) return AAX_ERROR_INVALID_CHUNK_INDEX;
        *id = kChunkID;
        return AAX_SUCCESS;
    }
    AAX_Result GetChunkSize(AAX_CTypeID id, uint32_t* size) const override {
        if (id != kChunkID) return AAX_CMonolithicParameters::GetChunkSize(id, size);
        *size = api()->save_state(instance_, nullptr, 0);
        return AAX_SUCCESS;
    }
    AAX_Result GetChunk(AAX_CTypeID id, AAX_SPlugInChunk* chunk) const override {
        if (id != kChunkID) return AAX_CMonolithicParameters::GetChunk(id, chunk);
        uint32_t size = api()->save_state(instance_, nullptr, 0);
        std::vector<uint8_t> data(size);
        if (api()->save_state(instance_, data.data(), size) != size) return AAX_ERROR_INCORRECT_CHUNK_SIZE;
        std::memcpy(chunk->fData, data.data(), size);
        chunk->fSize = (int32_t)size;
        return AAX_SUCCESS;
    }
    AAX_Result SetChunk(AAX_CTypeID id, const AAX_SPlugInChunk* chunk) override {
        if (id != kChunkID) return AAX_CMonolithicParameters::SetChunk(id, chunk);
        if (!api()->load_state(instance_, reinterpret_cast<const uint8_t*>(chunk->fData), (uint32_t)chunk->fSize))
            return AAX_ERROR_INVALID_CHUNK_ID;
        // Show the loaded values to Pro Tools.
        for (auto& [key, pid] : ids_) SetParameterNormalizedValue(key.c_str(), api()->get_param(instance_, pid));
        return AAX_SUCCESS;
    }
    AAX_Result CompareActiveChunk(const AAX_SPlugInChunk*, AAX_CBoolean* isEqual) const override {
        *isEqual = false;
        return AAX_SUCCESS;
    }

protected:
    void RenderAudio(AAX_SInstrumentRenderInfo* info, const TParamValPair* values[], int32_t count) override {
        const int32_t frames = *info->mNumSamples;
        events_.clear();
        // Parameter changes of this block, at its start.
        for (int32_t i = 0; i < count; i++) {
            const char* key = values[i]->first;
            double value = 0;
            if (!values[i]->second->GetValueAsDouble(&value)) continue;
            for (auto& [k, pid] : ids_) {
                if (k != key) continue;
                ferritron_event e{};
                e.offset = 0;
                e.type = FERRITRON_EVENT_PARAM;
                e.data.param.id = pid;
                // The Master Bypass is a bool; Ferritron's parameters are normalized here.
                e.data.param.normalized = (k == cDefaultMasterBypassID) ? (value != 0 ? 1.0 : 0.0) : value;
                if (events_.size() < maxEvents_) events_.push_back(e);
            }
        }
        // MIDI from the input node (instruments, and effects with `midi_input`).
        if (AAX_IMIDINode* node = info->mInputNode) {
            AAX_CMidiStream* stream = node->GetNodeBuffer();
            for (uint32_t i = 0; stream && i < stream->mBufferSize; i++) {
                const AAX_CMidiPacket& p = stream->mBuffer[i];
                const uint8_t status = p.mData[0] & 0xf0;
                ferritron_event e{};
                e.offset = std::min<uint32_t>(p.mTimestamp, (uint32_t)std::max(frames - 1, 0));
                if (status == 0x80 || status == 0x90) {
                    e.type = (status == 0x90 && p.mData[2] > 0) ? FERRITRON_EVENT_NOTE_ON : FERRITRON_EVENT_NOTE_OFF;
                    e.data.note.note_id = -1;
                    e.data.note.channel = p.mData[0] & 0x0f;
                    e.data.note.key = p.mData[1];
                    e.data.note.velocity = p.mData[2] / 127.0f;
                } else if (status >= 0xa0 && status <= 0xe0) {
                    e.type = FERRITRON_EVENT_MIDI;
                    e.data.midi.status = status;
                    e.data.midi.channel = p.mData[0] & 0x0f;
                    e.data.midi.data1 = p.mData[1] & 0x7f;
                    e.data.midi.data2 = p.mData[2] & 0x7f;
                } else {
                    continue;
                }
                if (events_.size() < maxEvents_) events_.push_back(e); // a block with more MIDI than kMaxMidi drops the rest
            }
            ferritron_sort_events(events_.data(), sortScratch_.data(), (uint32_t)events_.size());
        }
        // The host's transport.
        if (transport_ && info->mTransportNode) {
            if (AAX_ITransport* t = info->mTransportNode->GetTransport()) {
                ferritron_transport tr{};
                double tempo = 0;
                int32_t num = 0, den = 0;
                bool playing = false, looping = false;
                int64_t tick = 0, loop_start = 0, loop_end = 0, sample = 0;
                uint32_t per_quarter = 960000;
                t->GetTicksPerQuarter(&per_quarter);
                if (t->GetCurrentTempo(&tempo) == AAX_SUCCESS) {
                    tr.flags |= FERRITRON_TRANSPORT_TEMPO;
                    tr.tempo = tempo;
                }
                if (t->GetCurrentMeter(&num, &den) == AAX_SUCCESS) {
                    tr.flags |= FERRITRON_TRANSPORT_TIME_SIGNATURE;
                    tr.numerator = (uint16_t)num;
                    tr.denominator = (uint16_t)den;
                }
                if (t->IsTransportPlaying(&playing) == AAX_SUCCESS && playing) tr.flags |= FERRITRON_TRANSPORT_PLAYING;
                if (per_quarter > 0 && t->GetCurrentTickPosition(&tick) == AAX_SUCCESS) {
                    tr.flags |= FERRITRON_TRANSPORT_POSITION_BEATS;
                    tr.position_beats = (double)tick / (double)per_quarter;
                }
                if (t->GetCurrentNativeSampleLocation(&sample) == AAX_SUCCESS) {
                    tr.flags |= FERRITRON_TRANSPORT_POSITION_SAMPLES;
                    tr.position_samples = sample;
                }
                if (per_quarter > 0 && t->GetCurrentLoopPosition(&looping, &loop_start, &loop_end) == AAX_SUCCESS) {
                    tr.flags |= FERRITRON_TRANSPORT_LOOP_RANGE | (looping ? FERRITRON_TRANSPORT_LOOPING : 0);
                    tr.loop_start = (double)loop_start / (double)per_quarter;
                    tr.loop_end = (double)loop_end / (double)per_quarter;
                }
                transport_->set(instance_, &tr);
            }
        }
        const uint32_t channels = std::max<uint32_t>(1, channels_);
        const bool instrument = api()->info->kind == FERRITRON_PLUGIN_INSTRUMENT;
        float* out[32] = {};
        for (uint32_t c = 0; c < channels; c++) {
            out[c] = info->mAudioOutputs[c];
            if (instrument || !info->mAudioInputs) std::fill(out[c], out[c] + frames, 0.0f);
            else if (info->mAudioInputs[c] != out[c]) std::copy(info->mAudioInputs[c], info->mAudioInputs[c] + frames, out[c]);
        }
        // The MIDI output node (DescribeWithNodes); the host leaves it null if it has none.
        AAX_IMIDINode* const midi_out = info->mAdditionalInputMIDINodes[kMidiOutSlot];
        size_t next = 0;
        for (int32_t start = 0; start < frames; start += kMaxBlock) {
            const uint32_t n = (uint32_t)std::min(kMaxBlock, frames - start);
            size_t first = next;
            while (next < events_.size() && (events_[next].offset < (uint32_t)start + n || start + (int32_t)n == frames)) {
                events_[next].offset -= (uint32_t)start;
                next++;
            }
            float* block[32] = {};
            for (uint32_t c = 0; c < channels; c++) block[c] = out[c] + start;
            if (sidechain_) {
                // The host wrote the (mono) sidechain buffer's address into the sidechain slot (slot 0).
                const float* sc = reinterpret_cast<const float*>(info->mAdditionalInputMIDINodes[0]);
                const float* ptrs[1] = {sc ? sc + start : nullptr};
                if (sc) sidechain_->set(instance_, ptrs, 1);
                else sidechain_->set(instance_, nullptr, 0);
            }
            uint32_t out_count = 0;
            ferritron_process_data pd = {sizeof(pd), n, channels, (uint32_t)(next - first), block, events_.data() + first, nullptr, outEvents_, kMaxOut, &out_count};
            if (!api()->process(instance_, &pd)) {
                for (uint32_t c = 0; c < channels; c++) std::fill(block[c], block[c] + n, 0.0f);
            } else {
                HandleOutEvents(outEvents_, out_count, (uint32_t)start, frames, midi_out);
            }
        }
    }

private:
    static constexpr int32_t kMaxBlock = 1024;
    static constexpr size_t kMaxMidi = 2048; // the same as the VST3 shim's kMaxEvents
    static constexpr uint32_t kMaxOut = 256;
    static constexpr uint32_t kEdits = 256;
    void* instance_ = nullptr;
    std::vector<std::pair<std::string, uint32_t>> ids_;
    // Room for the events of one block; set in EffectInit (main thread), never grown on the render thread.
    size_t maxEvents_ = kMaxMidi;
    std::vector<ferritron_event> events_;
    // As long as maxEvents_ from the start, so the sort has room even before EffectInit.
    std::vector<ferritron_event> sortScratch_ = std::vector<ferritron_event>(kMaxMidi);
    // What the plugin writes for the host (out_events), filled by process.
    ferritron_event outEvents_[kMaxOut] = {};
    // Parameter edits the plugin made on the render thread, applied by ApplyEdits on the
    // host's timer (the main thread). One producer, one consumer; no locks.
    struct Edit {
        uint32_t id;
        double normalized;
    };
    Edit edits_[kEdits] = {};
    std::atomic<uint32_t> editsRead_{0};
    std::atomic<uint32_t> editsWrite_{0};
    uint32_t channels_ = 2;
    const ferritron_transport_api* transport_ = nullptr;
    const ferritron_main_api* main_ = nullptr;
    const ferritron_sidechain_api* sidechain_ = nullptr;

    // Render thread: what the plugin wrote in one piece of `start`'s block. Parameter edits are
    // queued for the timer; MIDI is posted to the output node. Other kinds (NOTE_END, ...) are
    // skipped: this shim does not know them.
    void HandleOutEvents(const ferritron_event* out, uint32_t count, uint32_t start, int32_t frames, AAX_IMIDINode* midi_out) {
        for (uint32_t i = 0; i < count && i < kMaxOut; i++) {
            switch (out[i].type) {
            case FERRITRON_EVENT_PARAM:
                QueueEdit(out[i].data.param.id, out[i].data.param.normalized);
                break;
            case FERRITRON_EVENT_MIDI:
                if (midi_out) PostMidi(out[i], start, frames, midi_out);
                break;
            default:
                break;
            }
        }
    }

    // Render thread: one MIDI message (status 0x80..0xE0, channel, two data bytes) as an AAX packet.
    // The offset is inside the piece; the packet's time is inside the audio buffer.
    static void PostMidi(const ferritron_event& e, uint32_t start, int32_t frames, AAX_IMIDINode* node) {
        const uint8_t status = e.data.midi.status & 0xf0;
        if (status < 0x80 || status > 0xe0) return;
        AAX_CMidiPacket packet{};
        const uint32_t at = start + e.offset;
        packet.mTimestamp = std::min<uint32_t>(at, (uint32_t)std::max(frames - 1, 0));
        packet.mData[0] = (uint8_t)(status | (e.data.midi.channel & 0x0f));
        packet.mData[1] = e.data.midi.data1 & 0x7f;
        packet.mData[2] = e.data.midi.data2 & 0x7f;
        // Program change and channel pressure have one data byte.
        packet.mLength = (status == 0xc0 || status == 0xd0) ? 2 : 3;
        node->PostMIDIPacket(&packet);
    }

    // Render thread: queues one edit; a full queue drops it (the value still changed in the plugin).
    void QueueEdit(uint32_t id, double normalized) {
        const uint32_t w = editsWrite_.load(std::memory_order_relaxed);
        const uint32_t next = (w + 1) % kEdits;
        if (next == editsRead_.load(std::memory_order_acquire)) return;
        edits_[w] = Edit{id, normalized};
        editsWrite_.store(next, std::memory_order_release);
    }

    // Main thread (the host's timer): tells Pro Tools the queued edits.
    void ApplyEdits() {
        uint32_t r = editsRead_.load(std::memory_order_relaxed);
        while (r != editsWrite_.load(std::memory_order_acquire)) {
            const Edit e = edits_[r];
            r = (r + 1) % kEdits;
            editsRead_.store(r, std::memory_order_release);
            for (auto& [key, pid] : ids_) {
                if (pid == e.id) SetParameterNormalizedValue(key.c_str(), e.normalized);
            }
        }
    }
};

// AAX_CMonolithicParameters::StaticDescribe, with two nodes it has no field for: the MIDI output
// (AAX_eMIDINodeType_LocalOutput) on the second slot of mAdditionalInputMIDINodes, and, when the
// plugin has one, a mono sidechain input (AAX_IComponentDescriptor::AddSideChainIn) on the first.
AAX_Result DescribeWithNodes(AAX_IEffectDescriptor* effect, const AAX_SInstrumentSetupInfo& setup, bool sidechain) {
    AAX_Result first_error = AAX_SUCCESS;
    auto check = [&](AAX_Result r) {
        if (r != AAX_SUCCESS && first_error == AAX_SUCCESS) first_error = r;
    };
    AAX_IComponentDescriptor* const comp = effect->NewComponentDescriptor();
    if (!comp) return AAX_ERROR_NULL_OBJECT;
    const auto global_node = AAX_FIELD_INDEX(AAX_SInstrumentRenderInfo, mGlobalNode);
    const auto input_node = AAX_FIELD_INDEX(AAX_SInstrumentRenderInfo, mInputNode);
    const auto transport_node = AAX_FIELD_INDEX(AAX_SInstrumentRenderInfo, mTransportNode);
    // Nodes the plugin does not use are filled with private data, to keep the context's layout.
    check(comp->AddPrivateData(global_node, sizeof(float), AAX_ePrivateDataOptions_DefaultOptions));
    if (setup.mNeedsInputMIDI) check(comp->AddMIDINode(input_node, AAX_eMIDINodeType_LocalInput, setup.mInputMIDINodeName, setup.mInputMIDIChannelMask));
    else check(comp->AddPrivateData(input_node, sizeof(float), AAX_ePrivateDataOptions_DefaultOptions));
    if (setup.mNeedsTransport) check(comp->AddMIDINode(transport_node, AAX_eMIDINodeType_Transport, "Transport", 0xffff));
    else check(comp->AddPrivateData(transport_node, sizeof(float), AAX_ePrivateDataOptions_DefaultOptions));
    const auto additional = (AAX_CFieldIndex)(offsetof(AAX_SInstrumentRenderInfo, mAdditionalInputMIDINodes) / sizeof(void*));
    for (int32_t i = 0; i < kMaxAdditionalMIDINodes; i++) {
        if (i == 0 && sidechain) check(comp->AddSideChainIn(additional));
        else if (i == kMidiOutSlot) check(comp->AddMIDINode(additional + i, AAX_eMIDINodeType_LocalOutput, "MIDI Out", 0xffff));
        else check(comp->AddPrivateData(additional + i, sizeof(float), AAX_ePrivateDataOptions_DefaultOptions));
    }
    check(comp->AddAudioIn(AAX_FIELD_INDEX(AAX_SInstrumentRenderInfo, mAudioInputs)));
    check(comp->AddAudioOut(AAX_FIELD_INDEX(AAX_SInstrumentRenderInfo, mAudioOutputs)));
    check(comp->AddAudioBufferLength(AAX_FIELD_INDEX(AAX_SInstrumentRenderInfo, mNumSamples)));
    check(comp->AddClock(AAX_FIELD_INDEX(AAX_SInstrumentRenderInfo, mClock)));
    check(comp->AddPrivateData(AAX_FIELD_INDEX(AAX_SInstrumentRenderInfo, mMeters), sizeof(float), AAX_ePrivateDataOptions_DefaultOptions));
    check(comp->AddPrivateData(AAX_FIELD_INDEX(AAX_SInstrumentRenderInfo, mPrivateData), sizeof(AAX_SInstrumentPrivateData), AAX_ePrivateDataOptions_DefaultOptions));
    check(comp->AddDataInPort(AAX_FIELD_INDEX(AAX_SInstrumentRenderInfo, mCurrentStateNum), sizeof(uint64_t)));
    AAX_IPropertyMap* const props = comp->NewPropertyMap();
    if (!props) return AAX_ERROR_NULL_OBJECT;
    check(props->AddProperty(AAX_eProperty_InputStemFormat, static_cast<int32_t>(setup.mInputStemFormat)));
    check(props->AddProperty(AAX_eProperty_OutputStemFormat, static_cast<int32_t>(setup.mOutputStemFormat)));
    check(props->AddProperty(AAX_eProperty_CanBypass, setup.mCanBypass));
    check(props->AddProperty(AAX_eProperty_Constraint_Location, 0x0 | AAX_eConstraintLocationMask_DataModel));
    if (sidechain) check(props->AddProperty(AAX_eProperty_SupportsSideChainInput, true));
    if (setup.mNeedsTransport) check(props->AddProperty(AAX_eProperty_UsesTransport, true));
    check(props->AddProperty(AAX_eProperty_ManufacturerID, static_cast<int32_t>(setup.mManufacturerID)));
    check(props->AddProperty(AAX_eProperty_ProductID, static_cast<int32_t>(setup.mProductID)));
    check(props->AddProperty(AAX_eProperty_PlugInID_Native, static_cast<int32_t>(setup.mPluginID)));
    // Instruments (and anything reading MIDI from the input node) are not multi-mono.
    if (!setup.mMultiMonoSupport) check(props->AddProperty(AAX_eProperty_Constraint_MultiMonoSupport, 0));
    check(comp->AddProcessProc_Native(AAX_CMonolithicParameters::StaticRenderAudio, props));
    check(effect->AddComponent(comp));
    return first_error;
}

}  // namespace

// The SDK's entry (AAX_Exports.cpp) asks for the plugin's descriptions here.
AAX_Result GetEffectDescriptions(AAX_ICollection* collection) {
    if (!api()) return AAX_ERROR_NULL_OBJECT;
    const ferritron_plugin_info& info = *api()->info;
    const std::string id = str(info.id), name = str(info.name), vendor = str(info.vendor), version = str(info.version);
    const bool instrument = info.kind == FERRITRON_PLUGIN_INSTRUMENT;

    const bool midi = instrument || (info.audio_flags & FERRITRON_AUDIO_MIDI_INPUT);
    // The default 1..2 range keeps 1.3.0's single stereo registration; any other range
    // gets one registration per stem format inside it.
    const bool default_range = info.min_channels <= 1 && info.max_channels == 2;
    AAX_Result result = AAX_SUCCESS;
    int registered = 0;
    for (const Stem& stem : kStems) {
        if (stem.channels < info.min_channels || stem.channels > info.max_channels) continue;
        if (default_range && stem.channels != 2) continue;
        AAX_IEffectDescriptor* desc = collection->NewDescriptor();
        if (!desc) return AAX_ERROR_NULL_OBJECT;
        desc->AddName(name.c_str());
        desc->AddCategory(instrument ? AAX_ePlugInCategory_SWGenerators : AAX_ePlugInCategory_Effect);

        AAX_SInstrumentSetupInfo setup;
        setup.mManufacturerID = code_of(vendor, 1);
        setup.mProductID = code_of(id, 2);
        // Stereo keeps the ID of 1.3.0; other stem formats are separate plug-in types.
        setup.mPluginID = code_of(id, stem.channels == 2 ? 3 : 100 + stem.channels);
        setup.mCanBypass = true;
        setup.mNeedsInputMIDI = midi;
        setup.mInputMIDINodeName = "MIDI In";
        setup.mInputMIDIChannelMask = 0xffff;
        setup.mNeedsTransport = true;
        setup.mInputStemFormat = instrument ? AAX_eStemFormat_None : stem.format;
        setup.mOutputStemFormat = stem.format;
        // Every registration gets the MIDI output node: the ABI does not say which plugins send MIDI.
        DescribeWithNodes(desc, setup, (info.audio_flags & FERRITRON_AUDIO_SIDECHAIN) != 0);

        desc->AddProcPtr((void*)Parameters::Create, kAAX_ProcPtrID_Create_EffectParameters);
        if (api()->get_extension(FERRITRON_EXT_VIEW)) desc->AddProcPtr((void*)Gui::Create, kAAX_ProcPtrID_Create_EffectGUI);

        const std::string effect_id = stem.channels == 2 ? id : id + "." + std::to_string(stem.channels);
        result = collection->AddEffect(effect_id.c_str(), desc);
        registered++;
    }
    if (registered == 0) return AAX_ERROR_NULL_OBJECT;
    collection->SetManufacturerName(vendor.c_str());
    collection->AddPackageName(name.c_str());
    int major = 0, minor = 0, patch = 0;
    std::sscanf(version.c_str(), "%d.%d.%d", &major, &minor, &patch);
    collection->SetPackageVersion((uint32_t)((major << 16) | (minor << 8) | patch));
    return result;
}
