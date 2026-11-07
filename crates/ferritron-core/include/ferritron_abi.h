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

#define FERRITRON_ABI_VERSION 1

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
} ferritron_plugin_info;

#define FERRITRON_ISOLATION_PER_INSTANCE (1u << 8)
#define FERRITRON_ISOLATION_PIPELINE (1u << 9)

enum ferritron_param_flags {
    FERRITRON_PARAM_LIST = 1 << 0,   /* discrete named choices */
    FERRITRON_PARAM_BYPASS = 1 << 1, /* the plugin's bypass switch (always present, listed last) */
    FERRITRON_PARAM_NOT_AUTOMATABLE = 1 << 2, /* not for automation (e.g. the AI Access switch) */
    FERRITRON_PARAM_HIDDEN = 1 << 3, /* not shown in the host's parameter lists (still saved) */
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
};

typedef struct ferritron_track_info {
    uint32_t flags;     /* FERRITRON_TRACK_HAS_* */
    ferritron_str name; /* UTF-8 */
    uint8_t color[3];   /* red, green, blue */
    uint32_t index;     /* position in the host's mixer, from 0 */
} ferritron_track_info;

typedef struct ferritron_track_api {
    void (*set_track_info)(void* instance, const ferritron_track_info* info); /* [main] */
} ferritron_track_api;

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
    /* Processes the channels in place. `events` must be sorted by offset;
       parameter changes take effect exactly at their sample (the block is
       split there). Returns false if the plugin failed; the shim must then
       output silence. */
    bool (*process)(void* instance, float* const* channels, uint32_t num_channels, uint32_t frames,
                    const ferritron_event* events, uint32_t num_events,
                    const ferritron_executor* executor /* NULL if the host has no thread pool */); /* [audio] */

    /* State. save_state returns the full size; data is written only if it fits in cap. */
    uint32_t (*save_state)(void* instance, uint8_t* buf, uint32_t cap); /* [main] */
    bool (*load_state)(void* instance, const uint8_t* data, uint32_t len); /* [main] */

    /* Optional features by ID (see "Extensions"); NULL if unsupported. */
    const void* (*get_extension)(const char* id); /* [any] */
} ferritron_plugin_api;

#ifdef __cplusplus
}
#endif

#endif /* FERRITRON_ABI_H */
