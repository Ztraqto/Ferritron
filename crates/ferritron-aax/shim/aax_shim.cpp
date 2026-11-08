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
// Written against Avid's documented API without the SDK at hand: never
// compiled or run. Pro Tools loads only PACE-signed plugins (wraptool) except
// in Pro Tools Developer.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

#include "ferritron_abi.h"

#include "AAX_CEffectGUI.h"
#include "AAX_CMonolithicParameters.h"
#include "AAX_CLinearTaperDelegate.h"
#include "AAX_CParameter.h"
#include "AAX_ICollection.h"
#include "AAX_IComponentDescriptor.h"
#include "AAX_IEffectDescriptor.h"
#include "AAX_IMIDINode.h"
#include "AAX_IPropertyMap.h"
#include "AAX_IViewContainer.h"
#include "AAX_IController.h"

#include <algorithm>
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
        host_ = ferritron_view_host{GetEffectParameters(), &Gui::get, &Gui::begin, &Gui::perform, &Gui::end};
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
        AAX_CSampleRate rate = 48000;
        Controller()->GetSampleRate(&rate);
        api()->activate(instance_, rate, kMaxBlock);
        if (auto* latency = static_cast<const ferritron_latency_api*>(api()->get_extension(FERRITRON_EXT_LATENCY)))
            Controller()->SetSignalLatency((int32_t)latency->get(instance_));
        return AAX_SUCCESS;
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
                events_.push_back(e);
            }
        }
        // Notes from the MIDI input (instruments).
        if (AAX_IMIDINode* node = info->mInputNode) {
            AAX_CMidiStream* stream = node->GetNodeBuffer();
            for (uint32_t i = 0; stream && i < stream->mBufferSize; i++) {
                const AAX_CMidiPacket& p = stream->mBuffer[i];
                uint8_t status = p.mData[0] & 0xf0;
                if (status != 0x80 && status != 0x90) continue;
                ferritron_event e{};
                e.offset = std::min<uint32_t>(p.mTimestamp, (uint32_t)std::max(frames - 1, 0));
                e.type = (status == 0x90 && p.mData[2] > 0) ? FERRITRON_EVENT_NOTE_ON : FERRITRON_EVENT_NOTE_OFF;
                e.data.note.note_id = -1;
                e.data.note.channel = p.mData[0] & 0x0f;
                e.data.note.key = p.mData[1];
                e.data.note.velocity = p.mData[2] / 127.0f;
                events_.push_back(e);
            }
            std::stable_sort(events_.begin(), events_.end(), [](const ferritron_event& a, const ferritron_event& b) { return a.offset < b.offset; });
        }
        float* out[2] = {info->mAudioOutputs[0], info->mAudioOutputs[1]};
        const bool instrument = api()->info->kind == FERRITRON_PLUGIN_INSTRUMENT;
        for (int c = 0; c < 2; c++) {
            if (instrument || !info->mAudioInputs) std::fill(out[c], out[c] + frames, 0.0f);
            else if (info->mAudioInputs[c] != out[c]) std::copy(info->mAudioInputs[c], info->mAudioInputs[c] + frames, out[c]);
        }
        size_t next = 0;
        for (int32_t start = 0; start < frames; start += kMaxBlock) {
            const uint32_t n = (uint32_t)std::min(kMaxBlock, frames - start);
            size_t first = next;
            while (next < events_.size() && (events_[next].offset < (uint32_t)start + n || start + (int32_t)n == frames)) {
                events_[next].offset -= (uint32_t)start;
                next++;
            }
            float* channels[2] = {out[0] + start, out[1] + start};
            if (!api()->process(instance_, channels, 2, n, events_.data() + first, (uint32_t)(next - first), nullptr)) {
                std::fill(channels[0], channels[0] + n, 0.0f);
                std::fill(channels[1], channels[1] + n, 0.0f);
            }
        }
    }

private:
    static constexpr int32_t kMaxBlock = 1024;
    void* instance_ = nullptr;
    std::vector<std::pair<std::string, uint32_t>> ids_;
    std::vector<ferritron_event> events_;
};

}  // namespace

// The SDK's entry (AAX_Exports.cpp) asks for the plugin's descriptions here.
AAX_Result GetEffectDescriptions(AAX_ICollection* collection) {
    if (!api()) return AAX_ERROR_NULL_OBJECT;
    const ferritron_plugin_info& info = *api()->info;
    const std::string id = str(info.id), name = str(info.name), vendor = str(info.vendor), version = str(info.version);
    const bool instrument = info.kind == FERRITRON_PLUGIN_INSTRUMENT;

    AAX_IEffectDescriptor* desc = collection->NewDescriptor();
    if (!desc) return AAX_ERROR_NULL_OBJECT;
    desc->AddName(name.c_str());
    desc->AddCategory(instrument ? AAX_ePlugInCategory_SWGenerators : AAX_ePlugInCategory_Effect);

    AAX_SInstrumentSetupInfo setup;
    setup.mManufacturerID = code_of(vendor, 1);
    setup.mProductID = code_of(id, 2);
    setup.mPluginID = code_of(id, 3);
    setup.mCanBypass = true;
    setup.mNeedsInputMIDI = instrument;
    setup.mInputMIDINodeName = "MIDI In";
    setup.mInputMIDIChannelMask = 0xffff;
    setup.mInputStemFormat = instrument ? AAX_eStemFormat_None : AAX_eStemFormat_Stereo;
    setup.mOutputStemFormat = AAX_eStemFormat_Stereo;
    AAX_CMonolithicParameters::StaticDescribe(desc, setup);

    desc->AddProcPtr((void*)Parameters::Create, kAAX_ProcPtrID_Create_EffectParameters);
    if (api()->get_extension(FERRITRON_EXT_VIEW)) desc->AddProcPtr((void*)Gui::Create, kAAX_ProcPtrID_Create_EffectGUI);

    AAX_Result result = collection->AddEffect(id.c_str(), desc);
    collection->SetManufacturerName(vendor.c_str());
    collection->AddPackageName(name.c_str());
    int major = 0, minor = 0, patch = 0;
    std::sscanf(version.c_str(), "%d.%d.%d", &major, &minor, &patch);
    collection->SetPackageVersion((uint32_t)((major << 16) | (minor << 8) | patch));
    return result;
}
