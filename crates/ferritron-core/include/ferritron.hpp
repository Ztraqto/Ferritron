/*
 * ferritron.hpp - write Ferritron plugins in C++ (C++20, header only).
 *
 * A plugin is a class with its metadata, a parameter table and a process
 * function; FERRITRON_PLUGIN generates the C ABI table (ferritron_abi.h)
 * from it, which the format adapters (VST3, CLAP, ...) use like any other
 * Ferritron plugin:
 *
 *     struct Gain {
 *         static constexpr ferritron::Info info{"com.example.gain", "Gain", "Example", "1.0.0"};
 *         static constexpr ferritron::Param params[]{{"gain", "Gain", "dB", -60.0, 12.0, 0.0}};
 *
 *         void process(ferritron::Block& block) {
 *             float g = ferritron::db_to_gain(block.param(0));
 *             for (auto ch : block.channels()) for (float& s : ch) s *= g;
 *         }
 *     };
 *     FERRITRON_PLUGIN(Gain)
 *
 * Optional members: `void activate(double sample_rate, uint32_t max_block)`,
 * `void reset()`, `uint32_t latency() const`. Kind defaults to effect.
 *
 * What the header does for you, as the Rust core does for Rust plugins:
 * parameter changes take effect at their exact sample (the block is split
 * there), a bypass parameter is added, state is saved and restored,
 * denormals are flushed while processing, and exceptions never cross the
 * ABI (the plugin then outputs silence).
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */
#ifndef FERRITRON_HPP
#define FERRITRON_HPP

#include "ferritron_abi.h"

#include <algorithm>
#include <atomic>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <span>
#include <string_view>
#if defined(_M_X64) || defined(__x86_64__)
#include <xmmintrin.h>
#endif

#ifdef _WIN32
#define FERRITRON_EXPORT extern "C" __declspec(dllexport)
#else
#define FERRITRON_EXPORT extern "C" __attribute__((visibility("default")))
#endif

namespace ferritron {

enum class Kind : uint32_t { Effect = FERRITRON_PLUGIN_EFFECT, Instrument = FERRITRON_PLUGIN_INSTRUMENT };

struct Info {
    std::string_view id; // reverse-domain identifier, never changes
    std::string_view name;
    std::string_view vendor;
    std::string_view version = "0.1.0";
    std::string_view url = "";
    std::string_view email = "";
    Kind kind = Kind::Effect;
};

/// A parameter: plain values from `min` to `max`, `steps` > 0 for stepped ones.
struct Param {
    std::string_view key; // stable: the parameter ID is derived from it
    std::string_view name;
    std::string_view unit;
    double min, max, def;
    uint32_t steps = 0;

    constexpr double normalize(double plain) const {
        double n = max > min ? (plain - min) / (max - min) : 0.0;
        return n < 0.0 ? 0.0 : n > 1.0 ? 1.0 : n;
    }
    constexpr double plain(double normalized) const {
        double v = min + (max - min) * (normalized < 0.0 ? 0.0 : normalized > 1.0 ? 1.0 : normalized);
        if (steps > 0) {
            double step = (max - min) / steps;
            v = min + step * static_cast<double>(static_cast<long long>((v - min) / step + 0.5));
        }
        return v;
    }
};

/// A note in the current block (instruments).
struct Note {
    uint32_t offset; // frame within the block
    bool on;
    int16_t key; // 60 = middle C
    float velocity;
};

inline float db_to_gain(double db) { return static_cast<float>(std::pow(10.0, db / 20.0)); }

/// Same IDs as the Rust core (FNV-1a), so a plugin keeps its automation
/// whichever language it is written in.
constexpr uint64_t fnv1a64(std::string_view bytes, uint64_t seed) {
    uint64_t hash = 0xcbf29ce484222325ull ^ seed;
    for (char c : bytes) {
        hash ^= static_cast<uint8_t>(c);
        hash *= 0x00000100000001b3ull;
    }
    return hash;
}

constexpr uint32_t param_id(std::string_view key) {
    uint64_t h = fnv1a64(key, 0);
    return static_cast<uint32_t>(h ^ (h >> 32)) & 0x7fffffffu;
}

constexpr uint32_t bypass_id = param_id("ferritron.bypass");

/// One stretch of the current block in which parameters are constant.
class Block {
public:
    Block(float* const* channels, uint32_t count, uint32_t start, uint32_t frames, const double* plain, std::span<const Note> notes)
        : channels_(channels), count_(count), start_(start), frames_(frames), plain_(plain), notes_(notes) {}

    uint32_t frames() const { return frames_; }
    uint32_t num_channels() const { return count_; }
    std::span<float> channel(uint32_t index) const { return {channels_[index] + start_, frames_}; }

    /// Every channel, for `for (auto ch : block.channels())`.
    struct Channels {
        const Block* block;
        struct It {
            const Block* block;
            uint32_t i;
            std::span<float> operator*() const { return block->channel(i); }
            It& operator++() { ++i; return *this; }
            bool operator!=(const It& other) const { return i != other.i; }
        };
        It begin() const { return {block, 0}; }
        It end() const { return {block, block->count_}; }
    };
    Channels channels() const { return {this}; }

    /// Plain value of parameter `index` (in the plugin's table) in this stretch.
    double param(size_t index) const { return plain_[index]; }

    /// Notes in this stretch; offsets are relative to it.
    std::span<const Note> notes() const { return notes_; }

private:
    float* const* channels_;
    uint32_t count_, start_, frames_;
    const double* plain_;
    std::span<const Note> notes_;
};

namespace detail {

constexpr ferritron_str str(std::string_view s) { return {s.data(), static_cast<uint32_t>(s.size())}; }

/// Flushes denormals while it lives (they are very slow on x86).
struct DenormalGuard {
#if defined(_M_X64) || defined(__x86_64__)
    unsigned saved = _mm_getcsr();
    DenormalGuard() { _mm_setcsr(saved | 0x8040); }
    ~DenormalGuard() { _mm_setcsr(saved); }
#endif
};

template <class P>
struct Adapter {
    static constexpr size_t count = std::size(P::params);
    static constexpr uint32_t max_notes = 512;

    struct Instance {
        P plugin{};
        std::atomic<double> normalized[count + 1]; // + bypass
        double plain[count]{};
        Note notes[max_notes];
        std::atomic<uint32_t> latency{0};
        bool failed = false;
    };

    static constexpr Param bypass{"ferritron.bypass", "Bypass", "", 0.0, 1.0, 0.0, 1};

    static const Param& param(size_t index) { return index < count ? P::params[index] : bypass; }
    static uint32_t id_of(size_t index) { return index < count ? param_id(P::params[index].key) : bypass_id; }
    static int index_of(uint32_t id) {
        for (size_t i = 0; i <= count; ++i)
            if (id_of(i) == id) return static_cast<int>(i);
        return -1;
    }
    static Instance* self(void* p) { return static_cast<Instance*>(p); }

    static void* create() {
        try {
            auto* inst = new Instance();
            for (size_t i = 0; i <= count; ++i) inst->normalized[i].store(param(i).normalize(param(i).def));
            return inst;
        } catch (...) {
            return nullptr;
        }
    }
    static void destroy(void* p) { delete self(p); }
    static uint32_t param_count() { return static_cast<uint32_t>(count + 1); }
    static bool param_info(uint32_t index, ferritron_param_info* out) {
        if (index > count || !out) return false;
        const Param& p = param(index);
        out->id = id_of(index);
        out->name = str(p.name);
        out->unit = str(p.unit);
        out->step_count = p.steps;
        out->default_normalized = p.normalize(p.def);
        out->flags = index == count ? FERRITRON_PARAM_BYPASS : 0;
        return true;
    }
    static double get_param(void* p, uint32_t id) {
        int i = index_of(id);
        return i < 0 ? 0.0 : self(p)->normalized[i].load(std::memory_order_relaxed);
    }
    static bool set_param(void* p, uint32_t id, double normalized) {
        int i = index_of(id);
        if (i < 0) return false;
        self(p)->normalized[i].store(std::clamp(normalized, 0.0, 1.0), std::memory_order_relaxed);
        return true;
    }
    static double to_plain(uint32_t id, double normalized) {
        int i = index_of(id);
        return i < 0 ? 0.0 : param(i).plain(normalized);
    }
    static double to_normalized(uint32_t id, double plain) {
        int i = index_of(id);
        return i < 0 ? 0.0 : param(i).normalize(plain);
    }
    static int32_t format_param(uint32_t id, double normalized, char* buf, uint32_t cap) {
        int i = index_of(id);
        if (i < 0 || !buf || cap == 0) return -1;
        const Param& p = param(i);
        double v = p.plain(normalized);
        int n;
        // The value only: hosts show the unit (param_info) next to it.
        if (static_cast<size_t>(i) == count) {
            n = std::snprintf(buf, cap, "%s", v >= 0.5 ? "On" : "Off");
        } else {
            n = std::snprintf(buf, cap, p.steps ? "%.0f" : "%.2f", v);
        }
        return n < 0 ? -1 : std::min<int32_t>(n, static_cast<int32_t>(cap) - 1);
    }
    static bool parse_param(uint32_t id, ferritron_str text, double* out) {
        int i = index_of(id);
        if (i < 0 || !out || !text.ptr) return false;
        char tmp[64]{};
        std::memcpy(tmp, text.ptr, std::min<size_t>(text.len, sizeof(tmp) - 1));
        if (static_cast<size_t>(i) == count) {
            *out = (tmp[0] == 'O' && tmp[1] == 'n') || tmp[0] == '1' ? 1.0 : 0.0;
            return true;
        }
        char* end = nullptr;
        double v = std::strtod(tmp, &end);
        if (end == tmp) return false;
        *out = param(i).normalize(v);
        return true;
    }
    static void reset(void* p) {
        auto* inst = self(p);
        try {
            if constexpr (requires(P& x) { x.reset(); }) inst->plugin.reset();
        } catch (...) {
            inst->failed = true;
        }
    }

    static bool activate(void* p, double sample_rate, uint32_t max_block) {
        auto* inst = self(p);
        try {
            if constexpr (requires(P& x) { x.activate(sample_rate, max_block); }) inst->plugin.activate(sample_rate, max_block);
            if constexpr (requires(P& x) { x.reset(); }) inst->plugin.reset();
            if constexpr (requires(const P& x) { x.latency(); }) inst->latency.store(inst->plugin.latency());
            inst->failed = false;
        } catch (...) {
            inst->failed = true;
        }
        return !inst->failed;
    }

    /// Runs the plugin on frames [start, end) with the current parameters.
    static void run(Instance* inst, float* const* channels, uint32_t n_channels, uint32_t start, uint32_t end, const ferritron_event* events,
                    uint32_t n_events) {
        if (end <= start) return;
        for (size_t i = 0; i < count; ++i) inst->plain[i] = P::params[i].plain(inst->normalized[i].load(std::memory_order_relaxed));
        uint32_t notes = 0;
        for (uint32_t e = 0; e < n_events && notes < max_notes; ++e) {
            const auto& ev = events[e];
            if ((ev.type == FERRITRON_EVENT_NOTE_ON || ev.type == FERRITRON_EVENT_NOTE_OFF) && ev.offset >= start && ev.offset < end)
                inst->notes[notes++] = {ev.offset - start, ev.type == FERRITRON_EVENT_NOTE_ON, ev.data.note.key, ev.data.note.velocity};
        }
        Block block(channels, n_channels, start, end - start, inst->plain, {inst->notes, notes});
        inst->plugin.process(block);
    }

    static bool process(void* p, const ferritron_process_data* data) {
        float* const* channels = data->channels;
        const uint32_t n_channels = data->num_channels, frames = data->frames, n_events = data->num_events;
        const ferritron_event* events = data->events;
        auto* inst = self(p);
        DenormalGuard guard;
        try {
            if (inst->failed) throw 0;
            uint32_t at = 0;
            for (uint32_t e = 0; e < n_events; ++e) {
                const auto& ev = events[e];
                if (ev.type != FERRITRON_EVENT_PARAM) continue;
                uint32_t offset = std::min(ev.offset, frames);
                if (offset > at && !bypassed(inst)) run(inst, channels, n_channels, at, offset, events, n_events);
                at = std::max(at, offset);
                set_param(inst, ev.data.param.id, ev.data.param.normalized);
            }
            if (bypassed(inst)) {
                if constexpr (P::info.kind == Kind::Instrument)
                    for (uint32_t c = 0; c < n_channels; ++c) std::fill(channels[c], channels[c] + frames, 0.0f);
            } else {
                run(inst, channels, n_channels, at, frames, events, n_events);
            }
            return true;
        } catch (...) {
            inst->failed = true;
            for (uint32_t c = 0; c < n_channels; ++c) std::fill(channels[c], channels[c] + frames, 0.0f);
            return false;
        }
    }
    static bool bypassed(Instance* inst) { return inst->normalized[count].load(std::memory_order_relaxed) >= 0.5; }

    /// State: "FRTC", then (id, normalized) for every parameter.
    static uint32_t save_state(void* p, uint8_t* buf, uint32_t cap) {
        const uint32_t size = 8 + static_cast<uint32_t>(count + 1) * 12;
        if (buf && cap >= size) {
            std::memcpy(buf, "FRTC", 4);
            uint32_t n = static_cast<uint32_t>(count + 1);
            std::memcpy(buf + 4, &n, 4);
            for (size_t i = 0; i <= count; ++i) {
                uint32_t id = id_of(i);
                double v = self(p)->normalized[i].load();
                std::memcpy(buf + 8 + i * 12, &id, 4);
                std::memcpy(buf + 12 + i * 12, &v, 8);
            }
        }
        return size;
    }
    static bool load_state(void* p, const uint8_t* data, uint32_t len) {
        if (!data || len < 8 || std::memcmp(data, "FRTC", 4) != 0) return false;
        uint32_t n;
        std::memcpy(&n, data + 4, 4);
        if (len < 8 + n * 12) return false;
        for (uint32_t i = 0; i < n; ++i) {
            uint32_t id;
            double v;
            std::memcpy(&id, data + 8 + i * 12, 4);
            std::memcpy(&v, data + 12 + i * 12, 8);
            set_param(p, id, v); // unknown IDs (removed parameters) are skipped
        }
        return true;
    }

    static uint32_t get_latency(void* p) { return self(p)->latency.load(); }
    static constexpr ferritron_latency_api latency_api{get_latency};
    static const void* get_extension(const char* id) {
        if (id && std::strcmp(id, FERRITRON_EXT_LATENCY) == 0) return &latency_api;
        return nullptr;
    }

    static constexpr uint8_t byte(uint64_t v, int i) { return static_cast<uint8_t>(v >> (56 - 8 * i)); }
    /// The same class ID as the Rust core computes from the plugin ID.
    static constexpr ferritron_plugin_info info() {
        const uint64_t a = fnv1a64(P::info.id, 0x666572726974726full), b = fnv1a64(P::info.id, 0x6e00000000000001ull);
        ferritron_plugin_info i{str(P::info.id), str(P::info.name), str(P::info.vendor), str(P::info.version), str(P::info.url),
                                str(P::info.email), static_cast<uint32_t>(P::info.kind), {}, 0};
        for (int k = 0; k < 8; ++k) {
            i.class_id[k] = byte(a, k);
            i.class_id[k + 8] = byte(b, k);
        }
        return i;
    }
    static constexpr ferritron_plugin_info info_value = info();

    static const ferritron_plugin_api* table() {
        static const ferritron_plugin_api api{
            sizeof(ferritron_plugin_api), FERRITRON_ABI_VERSION, &info_value, create, destroy, param_count, param_info, get_param,
            set_param, to_plain, to_normalized, format_param, parse_param, activate, process, save_state, load_state, get_extension,
            reset,
        };
        return &api;
    }
};

} // namespace detail
} // namespace ferritron

/// Exports `Plugin` as this library's Ferritron plugin.
#define FERRITRON_PLUGIN(Plugin)                                                                   \
    FERRITRON_EXPORT const ferritron_plugin_api* ferritron_plugin_entry(void) {                    \
        return ::ferritron::detail::Adapter<Plugin>::table();                                      \
    }

#endif
