/*
 * ferritron_abi.h - the stable C ABI between a Ferritron plugin core and the
 * format shims (VST3, CLAP, AU, AAX, ...).
 *
 * A shim is compiled against the official SDK of its format, which the plugin
 * developer provides. It talks to the plugin only through this header.
 *
 * Rules:
 *  - Structs start with struct_size / abi_version and only grow at the end.
 *  - Strings are UTF-8 and not null-terminated: see ferritron_str.
 *  - Functions marked [main] must not be called from the audio thread;
 *    [audio] functions are real-time safe; [any] may be called from anywhere.
 *  - Errors never unwind across this boundary; they are reported as `false`.
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */
#ifndef FERRITRON_ABI_H
#define FERRITRON_ABI_H

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define FERRITRON_ABI_VERSION 2

typedef struct ferritron_str {
    const char* ptr;
    uint32_t len;
} ferritron_str;

enum ferritron_plugin_kind {
    FERRITRON_PLUGIN_EFFECT = 0,     /* stereo in, stereo out */
    FERRITRON_PLUGIN_INSTRUMENT = 1, /* notes in, stereo out */
};

typedef struct ferritron_plugin_info {
    ferritron_str id; /* reverse-domain identifier */
    ferritron_str name;
    ferritron_str vendor;
    ferritron_str version;
    ferritron_str url;
    ferritron_str email;
    uint32_t kind; /* enum ferritron_plugin_kind */
    uint8_t class_id[16];
    /* Low byte: 0 = in-process (guarded), 1 = separate process. Flags:
       FERRITRON_ISOLATION_PER_INSTANCE (a process per instance instead of
       per plugin type), FERRITRON_ISOLATION_PIPELINE (blocks run one block
       late, reported as latency). */
    uint32_t isolation;
    /* Added in 1.3.1. Empty = the format's default. */
    ferritron_str vst3_subcategories; /* e.g. "Fx|EQ|Analyzer" */
    ferritron_str clap_features;      /* comma separated, e.g. "audio-effect,stereo,equalizer" */
    /* Added in 1.3.1: audio buses and MIDI. */
    uint32_t audio_flags;         /* FERRITRON_AUDIO_* */
    uint32_t min_channels;        /* main bus: fewest and most channels (1 and 2 unless the project says more) */
    uint32_t max_channels;
    uint32_t sidechain_channels;  /* with FERRITRON_AUDIO_SIDECHAIN: 0 = as many as the main bus */
    uint32_t vst2_id;             /* VST 2 plugin ID; 0 = derive one */
} ferritron_plugin_info;

#define FERRITRON_AUDIO_MIDI_INPUT (1u << 0)  /* an effect that takes MIDI */
#define FERRITRON_AUDIO_SIDECHAIN (1u << 1)   /* has a sidechain input */
#define FERRITRON_AUDIO_PASSTHROUGH (1u << 2) /* layouts out of range pass through instead of being refused */
#define FERRITRON_AUDIO_HOST_CC (1u << 3)     /* VST3: controllers go to the host's own mapping (no IMidiMapping) */

#define FERRITRON_ISOLATION_PER_INSTANCE (1u << 8)
#define FERRITRON_ISOLATION_PIPELINE (1u << 9)

enum ferritron_param_flags {
    FERRITRON_PARAM_LIST = 1 << 0,   /* discrete named choices */
    FERRITRON_PARAM_BYPASS = 1 << 1, /* the plugin's bypass switch (always present, listed last) */
    FERRITRON_PARAM_NOT_AUTOMATABLE = 1 << 2, /* not for automation (e.g. the AI Access switch) */
    FERRITRON_PARAM_HIDDEN = 1 << 3, /* not shown in the host's parameter lists (still saved) */
    FERRITRON_PARAM_PROGRAM = 1 << 4, /* (2.0) the built-in Program parameter of plugins with
                                         [presets]: an enum of the preset names */
    FERRITRON_PARAM_POLY = 1 << 5,    /* (2.0) poly = true: hosts may modulate it per note */
};

typedef struct ferritron_param_info {
    uint32_t id; /* stable, below 2^31 */
    ferritron_str name;
    ferritron_str unit;
    uint32_t step_count; /* 0 = continuous */
    double default_normalized;
    uint32_t flags; /* enum ferritron_param_flags */
} ferritron_param_info;

enum ferritron_event_type {
    FERRITRON_EVENT_PARAM = 0,
    FERRITRON_EVENT_NOTE_ON = 1,
    FERRITRON_EVENT_NOTE_OFF = 2,
    FERRITRON_EVENT_MIDI = 3, /* any other MIDI message (1.3.1) */
    FERRITRON_EVENT_NOTE_MOD = 4,        /* (2.0) per-note modulation: data.voice, target = parameter ID, value = plain offset */
    FERRITRON_EVENT_NOTE_EXPRESSION = 5, /* (2.0) note expression: data.voice, target = CLAP_NOTE_EXPRESSION_* number */
    FERRITRON_EVENT_NOTE_END = 6,        /* (2.0) plugin -> host only: the voice of a note ended; data.note (velocity 0). Shims that do not know it ignore it. */
};

/* A timed event inside the current block. */
typedef struct ferritron_event {
    uint32_t offset; /* 0 = first sample; params at >= frames apply after the block */
    uint32_t type;   /* enum ferritron_event_type */
    union {
        struct {
            uint32_t id;
            double normalized;
        } param;
        struct {
            int32_t note_id; /* -1 if unknown */
            int16_t channel; /* 0..15 */
            int16_t key;     /* 0..127, 60 = middle C */
            float velocity;  /* 0..1 */
        } note;
        struct {
            uint8_t status;  /* status byte without the channel: 0xB0 CC, 0xC0 program, 0xD0 channel pressure, 0xE0 pitch bend, 0xA0 poly pressure */
            uint8_t channel; /* 0..15 */
            uint8_t data1;
            uint8_t data2;
        } midi;
        struct {
            int32_t note_id; /* -1: any */
            uint32_t target;
            float value;
            int8_t key;      /* -1: any */
            int8_t channel;  /* -1: any */
            uint16_t reserved;
        } voice; /* all of note_id, key and channel -1: every note */
    } data;
} ferritron_event;

/* --- Extensions ------------------------------------------------------------
   Optional features are found with ferritron_plugin_api.get_extension(ID).
   Unknown IDs return NULL, so shims and plugins of different ages work
   together. New features are added as new IDs (or new versions of an ID),
   never by changing existing structs. */

#define FERRITRON_EXT_VIEW "ferritron.view.v1"
#define FERRITRON_EXT_LATENCY "ferritron.latency.v1"
#define FERRITRON_EXT_RENDER "ferritron.render.v1"
#define FERRITRON_EXT_HOST "ferritron.host.v1"
#define FERRITRON_EXT_EDITOR_SIZE "ferritron.editor-size.v1"
#define FERRITRON_EXT_ARA "ferritron.ara.v1"
#define FERRITRON_EXT_SLEEP "ferritron.sleep.v1"
#define FERRITRON_EXT_HOST2 "ferritron.host.v2"
#define FERRITRON_EXT_TRACK "ferritron.track.v1"
#define FERRITRON_EXT_TRANSPORT "ferritron.transport.v1"
#define FERRITRON_EXT_MAIN "ferritron.main.v1"
/* (2.0) How many voices the instrument plays (Process::VOICES); absent when it does not say. */
#define FERRITRON_EXT_VOICES "ferritron.voices.v1"
typedef struct ferritron_voices_api {
    uint32_t (*capacity)(void* instance); /* [main] */
} ferritron_voices_api;
/* (2.0, audit fix f4) What an instance is for. The VST3 shim calls set_role(instance, FERRITRON_ROLE_CONTROLLER) right
   after create for the controller copy of a [vst3] controller_id plugin: that copy is then not listed by
   the instance lists and not served by the MCP hub. Cores or shims without this extension behave as before
   (the extension is NULL, the shim does not call it). Check struct_size before calling. */
#define FERRITRON_EXT_ROLE "ferritron.role.v1"
#define FERRITRON_ROLE_NORMAL 0
#define FERRITRON_ROLE_CONTROLLER 1
typedef struct ferritron_role_api {
    uint32_t struct_size;
    void (*set_role)(void* instance, uint32_t role); /* [main] */
} ferritron_role_api;
#define FERRITRON_EXT_SIDECHAIN "ferritron.sidechain.v1"
#define FERRITRON_EXT_LAYOUT "ferritron.layout.v1"
#define FERRITRON_EXT_DESCRIBE "ferritron.describe.v1"

/* FERRITRON_EXT_DESCRIBE: the plugin described as UTF-8 JSON, for shims
   that show it by other means than this ABI's numbers (WAM in the browser,
   the standalone apps on phones): the plugin's id, name, vendor, version,
   kind and language, its parameters with their keys (stable, from the
   project file), kinds, ranges, localized names and choices, and the
   editor's messages. Writes the text into buf if it fits in cap and
   returns its full length. */
typedef struct ferritron_describe_api {
    uint32_t (*describe)(char* buf, uint32_t cap); /* [main] */
} ferritron_describe_api;

/* FERRITRON_EXT_SLEEP: silence. A plugin stops processing (sleeps) once its
   input has been silent for as long as its output can go on; input, notes
   and parameter changes wake it. tail() is that length in samples, latency
   included, or FERRITRON_TAIL_UNKNOWN when the plugin decides by itself (or
   never stops); shims report it to the host. silent() is true when the last
   process() call slept and left silence in the channels; shims tell the host
   (VST3 silence flags, CLAP_PROCESS_SLEEP). */
#define FERRITRON_TAIL_UNKNOWN 0xFFFFFFFFu
typedef struct ferritron_sleep_api {
    uint32_t (*tail)(void* instance);  /* [any] */
    bool (*silent)(void* instance);    /* [audio] */
} ferritron_sleep_api;

/* FERRITRON_EXT_ARA: ARA 2 (Audio Random Access). ARA types are passed as
   plain pointers, so this header needs no ARA SDK. Shims built with the ARA
   SDK publish the factory (VST3: an "ARA Main Factory Class" and
   IPlugInEntryPoint2; CLAP: the ARA factory and plug-in extension), bind
   instances, and call render() before process() with the song position. */
typedef struct ferritron_ara_api {
    const void* (*get_factory)(void); /* const ARAFactory* */ /* [any] */
    /* returns const ARAPlugInExtensionInstance* */
    const void* (*bind)(void* instance, void* document_controller_ref, uint32_t known_roles,
                        uint32_t assigned_roles); /* [main] */
    void (*set_rendering)(void* instance, bool enable, double sample_rate, uint32_t channels,
                          uint32_t max_frames); /* [main] */
    /* Renders the instance's playback regions into the channels (replacing
       them) at song position `position` in samples. Returns false if the
       instance is no playback renderer: the channels are left as they are. */
    bool (*render)(void* instance, float* const* channels, uint32_t num_channels, uint32_t frames,
                   int64_t position, bool playing); /* [audio] */
    void (*release)(void* instance); /* called by the plugin itself on destroy; shims do not */
} ferritron_ara_api;

/* FERRITRON_EXT_EDITOR_SIZE (plugins with a view): the editor size the user
   chose, in logical pixels, saved and restored with the plugin state. Shims
   call set() when the editor is resized and get() before opening it; get()
   returns false while no size was chosen (use the view's default). */
typedef struct ferritron_editor_size_api {
    bool (*get)(void* instance, uint32_t* width, uint32_t* height); /* [main] */
    void (*set)(void* instance, uint32_t width, uint32_t height);   /* [main] */
} ferritron_editor_size_api;

/* What the shim gives the plugin about its host. edited() tells the host
   that the plugin changed a parameter by itself (e.g. through the MCP hub);
   it may be called from any thread, and the shim delivers begin/perform/end
   edit to the host on the right one. */
typedef struct ferritron_host_callbacks {
    void* ctx;
    void (*edited)(void* ctx, uint32_t id, double normalized);
} ferritron_host_callbacks;

/* FERRITRON_EXT_HOST */
typedef struct ferritron_host_api {
    /* `host` is copied; ctx must stay valid until the instance is destroyed. */
    void (*attach)(void* instance, const ferritron_host_callbacks* host); /* [main] */
    /* The track (mixer channel) the instance sits on, when the host says. */
    void (*set_track_name)(void* instance, ferritron_str name);          /* [main] */
} ferritron_host_api;

/* FERRITRON_EXT_HOST2: what plugin code may ask of the host from any thread
   (Ferritron's InstanceHandle). A shim that offers it attaches with it after
   (or instead of) FERRITRON_EXT_HOST. edit() reports one step of an edit
   the plugin made by itself, as a gesture: FERRITRON_EDIT_BEGIN, then
   FERRITRON_EDIT_PERFORM per value, then FERRITRON_EDIT_END; the shim hands
   them to the host on the right thread, in order. request_process() asks
   the host to call process() soon although the output was silent (CLAP:
   host->request_process); NULL where hosts keep calling process anyway.
   ctx must be the same pointer the shim passes as ferritron_view_host::ctx
   to this instance's editor: that is how an editor finds its instance. */
enum { FERRITRON_EDIT_BEGIN = 0, FERRITRON_EDIT_PERFORM = 1, FERRITRON_EDIT_END = 2 };

typedef struct ferritron_host_callbacks2 {
    void* ctx;
    void (*edit)(void* ctx, uint32_t kind, uint32_t id, double normalized); /* [any] */
    void (*request_process)(void* ctx);                                       /* [any], may be NULL */
} ferritron_host_callbacks2;

typedef struct ferritron_host_api2 {
    /* `host` is copied; ctx must stay valid until the instance is destroyed. */
    void (*attach)(void* instance, const ferritron_host_callbacks2* host); /* [main] */
} ferritron_host_api2;

/* FERRITRON_EXT_TRACK: the track (mixer channel) the instance sits on, with
   whatever the host tells of it. Shims call it when they first learn the
   track and whenever the host says it changed. */
enum {
    FERRITRON_TRACK_HAS_NAME = 1 << 0,
    FERRITRON_TRACK_HAS_COLOR = 1 << 1,
    FERRITRON_TRACK_HAS_INDEX = 1 << 2,
    FERRITRON_TRACK_HAS_KIND = 1 << 3,     /* (2.0) */
    FERRITRON_TRACK_HAS_LOCATION = 1 << 4, /* (2.0) */
    FERRITRON_TRACK_HAS_CHANNELS = 1 << 5, /* (2.0) */
};

typedef struct ferritron_track_info {
    uint32_t flags;     /* FERRITRON_TRACK_HAS_* */
    ferritron_str name; /* UTF-8 */
    uint8_t color[3];   /* red, green, blue */
    uint32_t index;     /* position in the host's mixer, from 0 */
    uint32_t kind;      /* (2.0) FERRITRON_TRACK_HAS_KIND: 0 track, 1 return, 2 bus, 3 master */
    uint32_t location;  /* (2.0) FERRITRON_TRACK_HAS_LOCATION: 0 pre fader, 1 post fader, 2 panner */
    uint32_t channels;  /* (2.0) FERRITRON_TRACK_HAS_CHANNELS: audio channels of the track */
} ferritron_track_info;

typedef struct ferritron_track_api {
    void (*set_track_info)(void* instance, const ferritron_track_info* info); /* [main] */
} ferritron_track_api;

/* FERRITRON_EXT_TRANSPORT (1.3.1): the host's transport. Shims call set()
   on the audio thread right before every process(). */
#define FERRITRON_TRANSPORT_TEMPO (1u << 0)
#define FERRITRON_TRANSPORT_TIME_SIGNATURE (1u << 1)
#define FERRITRON_TRANSPORT_PLAYING (1u << 2)
#define FERRITRON_TRANSPORT_RECORDING (1u << 3)
#define FERRITRON_TRANSPORT_POSITION_BEATS (1u << 4)
#define FERRITRON_TRANSPORT_BAR_START (1u << 5)
#define FERRITRON_TRANSPORT_POSITION_SAMPLES (1u << 6)
#define FERRITRON_TRANSPORT_LOOP_RANGE (1u << 7)
#define FERRITRON_TRANSPORT_LOOPING (1u << 8)

typedef struct ferritron_transport {
    uint32_t flags; /* FERRITRON_TRANSPORT_*: which fields are valid */
    uint16_t numerator;
    uint16_t denominator;
    double tempo;
    double position_beats;
    double bar_start_beats;
    int64_t position_samples;
    double loop_start;
    double loop_end;
} ferritron_transport;

typedef struct ferritron_transport_api {
    void (*set)(void* instance, const ferritron_transport* transport); /* [audio] */
} ferritron_transport_api;

/* FERRITRON_EXT_MAIN (1.3.1): work the plugin asked of the host's main thread
   from other threads. Shims call take_requests() on the main thread
   regularly (a timer, or the host's callback request) and act on the bits:
   LATENCY: the latency changed (read it with FERRITRON_EXT_LATENCY and tell
   the host: VST3 restartComponent(kLatencyChanged); CLAP request_restart and
   latency->changed()); VALUES: many parameter values changed at once (VST3
   restartComponent(kParamValuesChanged); CLAP params->rescan VALUES). */
#define FERRITRON_REQUEST_LATENCY (1u << 0)
#define FERRITRON_REQUEST_VALUES (1u << 1)
/* (2.0) InstanceHandle::notify codes wait; take_requests runs their handler itself and never
   returns this bit. */
#define FERRITRON_REQUEST_NOTIFY (1u << 31)

typedef struct ferritron_main_api {
    uint32_t (*take_requests)(void* instance); /* [main] returns and clears the bits */
    bool (*pending)(void* instance);           /* [audio, any] any bit set? does not clear */
} ferritron_main_api;

/* FERRITRON_EXT_SIDECHAIN (1.3.1): the sidechain input of the next block. Shims call set()
   on the audio thread right before every process(); n = 0 or channels = NULL means not
   connected. The buffers (read-only) must stay valid until process() returns. */
typedef struct ferritron_sidechain_api {
    void (*set)(void* instance, const float* const* channels, uint32_t n); /* [audio] */
} ferritron_sidechain_api;

/* FERRITRON_EXT_LAYOUT (1.3.1): the speaker of each channel of the main bus, as FERRITRON_SPEAKER_*
   ids (0..29 in the order below; 0x1000 + n for a position without a name). Shims call it before
   activate whenever the host sets the arrangement. */
enum {
    FERRITRON_SPEAKER_L = 0, FERRITRON_SPEAKER_R, FERRITRON_SPEAKER_C, FERRITRON_SPEAKER_LFE, FERRITRON_SPEAKER_LS,
    FERRITRON_SPEAKER_RS, FERRITRON_SPEAKER_LSS, FERRITRON_SPEAKER_RSS, FERRITRON_SPEAKER_LSR, FERRITRON_SPEAKER_RSR,
    FERRITRON_SPEAKER_LC, FERRITRON_SPEAKER_RC, FERRITRON_SPEAKER_CS, FERRITRON_SPEAKER_TC, FERRITRON_SPEAKER_TFL,
    FERRITRON_SPEAKER_TFC, FERRITRON_SPEAKER_TFR, FERRITRON_SPEAKER_TRL, FERRITRON_SPEAKER_TRC, FERRITRON_SPEAKER_TRR,
    FERRITRON_SPEAKER_TSL, FERRITRON_SPEAKER_TSR, FERRITRON_SPEAKER_LW, FERRITRON_SPEAKER_RW, FERRITRON_SPEAKER_BFL,
    FERRITRON_SPEAKER_BFC, FERRITRON_SPEAKER_BFR, FERRITRON_SPEAKER_BRL, FERRITRON_SPEAKER_BRR, FERRITRON_SPEAKER_LFE2,
    FERRITRON_SPEAKER_OTHER = 0x1000,
};

typedef struct ferritron_layout_api {
    void (*set)(void* instance, const uint32_t* speakers, uint32_t n); /* [main] */
} ferritron_layout_api;

/* FERRITRON_EXT_RENDER: whether the host renders offline (bounce, export).
   The plugin's processing quality follows at once; its setup (Setup::offline)
   at the next activate, so shims reactivate when it changes. Shims may also
   call it from the audio thread when the host says per block. */
typedef struct ferritron_render_api {
    void (*set_offline)(void* instance, bool offline); /* [main] [audio] */
} ferritron_render_api;

/* FERRITRON_EXT_LATENCY: the delay the host should compensate, in samples.
   Valid after activate; it changes only when activate is called again. */
typedef struct ferritron_latency_api {
    uint32_t (*get)(void* instance); /* [main] */
} ferritron_latency_api;

/* Callbacks from a view to the shim. All are called on the UI thread. */
typedef struct ferritron_view_host {
    void* ctx;
    double (*get_param)(void* ctx, uint32_t id); /* normalized */
    void (*begin_edit)(void* ctx, uint32_t id);
    void (*perform_edit)(void* ctx, uint32_t id, double normalized); /* sets the value and tells the host */
    void (*end_edit)(void* ctx, uint32_t id);
    /* Asks the host window to become width x height physical pixels (it answers through
       ferritron_view_api::set_size). NULL where the format cannot resize on request. */
    bool (*request_resize)(void* ctx, uint32_t width, uint32_t height);
} ferritron_view_host;

/* FERRITRON_EXT_VIEW: the plugin's editor, embedded in a host window.
   Sizes in this struct are logical pixels (at 100% scale). Sizes passed to
   open / set_size are physical pixels of the host window, together with the
   scale factor the shim determined (host-provided, or the window's DPI). */
typedef struct ferritron_view_api {
    uint32_t width; /* default size */
    uint32_t height;
    uint32_t min_width; /* resize limits; min == max when not resizable */
    uint32_t min_height;
    uint32_t max_width;
    uint32_t max_height;
    /* parent: HWND on Windows. `host` must stay valid until close. NULL on failure. */
    void* (*open)(void* parent, const ferritron_view_host* host, uint32_t width, uint32_t height,
                  double scale);                                                   /* [main] */
    void (*close)(void* view);                                                     /* [main] */
    void (*set_size)(void* view, uint32_t width, uint32_t height, double scale); /* [main] */
} ferritron_view_api;

/* A host thread pool a shim offers for one process() call. run() executes
   task(data, i) for i in 0..count (possibly in parallel) and returns true once
   all finished, or returns false without running anything. */
typedef struct ferritron_executor {
    void* ctx;
    bool (*run)(void* ctx, uint32_t count, void (*task)(void* data, uint32_t index), void* data);
} ferritron_executor;

/* Everything one process() call gets. `size` is sizeof() as the caller built
   it: a field at or beyond `size` is not there (so fields can be added later).
   Fields up to and including `executor` must be present. */
typedef struct ferritron_process_data {
    uint32_t size;
    uint32_t frames;
    uint32_t num_channels;
    uint32_t num_events;
    float* const* channels;
    const ferritron_event* events; /* sorted by offset */
    const ferritron_executor* executor; /* NULL if the host has no thread pool */
    ferritron_event* out_events; /* events the plugin may write (MIDI out, notifications); NULL = no room */
    uint32_t out_capacity;
    uint32_t* out_count; /* the plugin writes how many of out_events it filled; NULL = no room */
} ferritron_process_data;

typedef struct ferritron_plugin_api {
    uint32_t struct_size;
    uint32_t abi_version;
    const ferritron_plugin_info* info;

    /* Instances */
    void* (*create)(void);                 /* [main] NULL on failure */
    void (*destroy)(void* instance);       /* [main] */

    /* Parameters. Values crossing the ABI are normalized to 0..1. */
    uint32_t (*param_count)(void);                                          /* [any] */
    bool (*param_info)(uint32_t index, ferritron_param_info* out);          /* [any] */
    double (*get_param)(void* instance, uint32_t id);                       /* [any] */
    bool (*set_param)(void* instance, uint32_t id, double normalized);      /* [any] */
    double (*to_plain)(uint32_t id, double normalized);                     /* [any] */
    double (*to_normalized)(uint32_t id, double plain);                     /* [any] */
    /* Writes UTF-8 display text into buf and returns its length, or -1. */
    int32_t (*format_param)(uint32_t id, double normalized, char* buf, uint32_t cap); /* [main] */
    bool (*parse_param)(uint32_t id, ferritron_str text, double* normalized);        /* [main] */

    /* Processing */
    bool (*activate)(void* instance, double sample_rate, uint32_t max_block_size); /* [main] */
    /* Processes the channels in place. `data->events` must be sorted by
       offset; with the default `[process] automation = "control"` parameter
       changes take effect at the start of their control period (the block is
       split there; "sample" splits at the exact sample). Returns false if the
       plugin failed; the shim must then output silence. */
    bool (*process)(void* instance, const ferritron_process_data* data); /* [audio] */

    /* State. save_state returns the full size; data is written only if it fits in cap. */
    uint32_t (*save_state)(void* instance, uint8_t* buf, uint32_t cap); /* [main] */
    bool (*load_state)(void* instance, const uint8_t* data, uint32_t len); /* [main] */

    /* Optional features by ID (see "Extensions"); NULL if unsupported. */
    const void* (*get_extension)(const char* id); /* [any] */
    /* (2.0.1) clears delay lines, voices and smoothing; never while process runs. Check struct_size before calling. */
    void (*reset)(void* instance); /* [audio] */
} ferritron_plugin_api;

/* Layout guard: the same sizes are asserted in abi.rs (abi_sizes_match_the_c_header).
   Change both together; never change the layout of an existing struct. */
#if defined(__cplusplus)
#define FERRITRON_ABI_ASSERT(cond, msg) static_assert(cond, msg)
#else
#define FERRITRON_ABI_ASSERT(cond, msg) _Static_assert(cond, msg)
#endif
#if UINTPTR_MAX == 0xffffffffffffffffu
FERRITRON_ABI_ASSERT(sizeof(ferritron_event) == 24, "ferritron_event");
FERRITRON_ABI_ASSERT(sizeof(ferritron_process_data) == 64, "ferritron_process_data");
FERRITRON_ABI_ASSERT(sizeof(ferritron_param_info) == 64, "ferritron_param_info");
FERRITRON_ABI_ASSERT(sizeof(ferritron_plugin_info) == 176, "ferritron_plugin_info");
FERRITRON_ABI_ASSERT(sizeof(ferritron_plugin_api) == 144, "ferritron_plugin_api");
FERRITRON_ABI_ASSERT(sizeof(ferritron_transport) == 56, "ferritron_transport");
#endif
#undef FERRITRON_ABI_ASSERT

#ifdef __cplusplus
}
#endif

#endif /* FERRITRON_ABI_H */
