// CLAP editor checks, included at the end of host.cpp when the CLAP headers
// are available. Mirrors the VST3 --gui checks through the CLAP gui and
// params extensions.

#include <clap/clap.h>

namespace {

struct EventList {
	std::vector<std::vector<uint8_t>> events;

	static uint32_t size(const clap_input_events_t* l) {
		return static_cast<uint32_t>(static_cast<EventList*>(l->ctx)->events.size());
	}
	static const clap_event_header_t* get(const clap_input_events_t* l, uint32_t i) {
		return reinterpret_cast<const clap_event_header_t*>(static_cast<EventList*>(l->ctx)->events[i].data());
	}
	static bool push(const clap_output_events_t* l, const clap_event_header_t* e) {
		auto* p = reinterpret_cast<const uint8_t*>(e);
		static_cast<EventList*>(l->ctx)->events.emplace_back(p, p + e->size);
		return true;
	}

	clap_input_events_t in{this, size, get};
	clap_output_events_t out{this, push};
};

bool g_flushRequested = false;

void hostRescan(const clap_host_t*, clap_param_rescan_flags) {}
void hostClear(const clap_host_t*, clap_id, clap_param_clear_flags) {}
void hostRequestFlush(const clap_host_t*) { g_flushRequested = true; }
const clap_host_params_t g_hostParams{hostRescan, hostClear, hostRequestFlush};

const void* hostGetExtension(const clap_host_t*, const char* id) {
	return std::strcmp(id, CLAP_EXT_PARAMS) == 0 ? &g_hostParams : nullptr;
}
void hostRequest(const clap_host_t*) {}

int g_restarts = 0;
void hostRequestRestart(const clap_host_t*) { ++g_restarts; }

} // namespace

// Render mode and latency through the CLAP render and latency extensions:
// the latency in real time and offline, and that switching the mode while
// active asks the host for a restart.
extern "C" int ferritron_testhost_clap_render(const wchar_t* binary) {
	HMODULE module = LoadLibraryW(binary);
	if (!module)
		return fail("cannot load the CLAP binary");
	auto* entry = reinterpret_cast<const clap_plugin_entry_t*>(GetProcAddress(module, "clap_entry"));
	if (!entry || !entry->init("plugin"))
		return fail("no clap_entry");
	auto* factory = static_cast<const clap_plugin_factory_t*>(entry->get_factory(CLAP_PLUGIN_FACTORY_ID));
	const clap_plugin_descriptor_t* desc = factory->get_plugin_descriptor(factory, 0);
	std::printf("plugin: %s (CLAP)\n", desc->name);
	clap_host_t host{CLAP_VERSION_INIT, nullptr, "ferritron-testhost", "Ferritron", "", "0",
	                 hostGetExtension, hostRequestRestart, hostRequest, hostRequest};

	auto create = [&](clap_plugin_render_mode mode) -> const clap_plugin_t* {
		const clap_plugin_t* plugin = factory->create_plugin(factory, &host, desc->id);
		if (!plugin || !plugin->init(plugin))
			return nullptr;
		auto* render = static_cast<const clap_plugin_render_t*>(plugin->get_extension(plugin, CLAP_EXT_RENDER));
		if (render)
			render->set(plugin, mode);
		plugin->activate(plugin, 48000.0, 256, 256);
		return plugin;
	};
	auto latencyOf = [](const clap_plugin_t* plugin) {
		auto* latency = static_cast<const clap_plugin_latency_t*>(plugin->get_extension(plugin, CLAP_EXT_LATENCY));
		return latency ? latency->get(plugin) : 0u;
	};

	for (clap_plugin_render_mode mode : {CLAP_RENDER_REALTIME, CLAP_RENDER_OFFLINE}) {
		const clap_plugin_t* plugin = create(mode);
		if (!plugin)
			return fail("create_plugin failed");
		std::printf("%s: latency %u samples\n", mode == CLAP_RENDER_OFFLINE ? "offline" : "realtime", latencyOf(plugin));
		plugin->deactivate(plugin);
		plugin->destroy(plugin);
	}

	// Switching while active: the plugin asks for a restart, the host
	// reactivates, and the new mode applies.
	const clap_plugin_t* plugin = create(CLAP_RENDER_REALTIME);
	if (!plugin)
		return fail("create_plugin failed");
	g_restarts = 0;
	auto* render = static_cast<const clap_plugin_render_t*>(plugin->get_extension(plugin, CLAP_EXT_RENDER));
	if (render)
		render->set(plugin, CLAP_RENDER_OFFLINE);
	std::printf("switched to offline while active: restart requested: %s\n", g_restarts ? "yes" : "no");
	plugin->deactivate(plugin);
	plugin->activate(plugin, 48000.0, 256, 256);
	std::printf("after the restart: latency %u samples\n", latencyOf(plugin));
	plugin->deactivate(plugin);
	plugin->destroy(plugin);
	entry->deinit();
	FreeLibrary(module);
	return 0;
}

extern "C" int ferritron_testhost_clap_gui(const wchar_t* binary, const wchar_t* shotDir, int clickX, int clickY,
                                           double scale, int resizeW, int resizeH) {
	HMODULE module = LoadLibraryW(binary);
	if (!module)
		return fail("cannot load the CLAP binary");
	auto* entry = reinterpret_cast<const clap_plugin_entry_t*>(GetProcAddress(module, "clap_entry"));
	if (!entry || !entry->init("plugin"))
		return fail("no clap_entry");
	auto* factory = static_cast<const clap_plugin_factory_t*>(entry->get_factory(CLAP_PLUGIN_FACTORY_ID));
	const clap_plugin_descriptor_t* desc = factory->get_plugin_descriptor(factory, 0);
	std::printf("plugin: %s (CLAP)\n", desc->name);

	clap_host_t host{CLAP_VERSION_INIT, nullptr, "ferritron-testhost", "Ferritron", "", "0",
	                 hostGetExtension, hostRequest, hostRequest, hostRequest};
	const clap_plugin_t* plugin = factory->create_plugin(factory, &host, desc->id);
	if (!plugin || !plugin->init(plugin))
		return fail("create_plugin failed");
	auto* params = static_cast<const clap_plugin_params_t*>(plugin->get_extension(plugin, CLAP_EXT_PARAMS));
	auto* gui = static_cast<const clap_plugin_gui_t*>(plugin->get_extension(plugin, CLAP_EXT_GUI));
	if (!params || !gui)
		return fail("missing params or gui extension");
	clap_param_info_t info{};
	params->get_info(plugin, 0, &info);

	OleInitialize(nullptr);
	uint32_t w = 0, h = 0;
	if (!gui->create(plugin, CLAP_WINDOW_API_WIN32, false))
		return fail("gui create failed");
	if (scale > 0)
		std::printf("host: set_scale %.2f -> %s\n", scale, gui->set_scale(plugin, scale) ? "applied" : "ignored");
	else
		scale = 1.0;
	if (!gui->get_size(plugin, &w, &h))
		return fail("gui get_size failed");
	std::printf("editor: %ux%u (resizable: %s)\n", w, h, gui->can_resize(plugin) ? "yes" : "no");
	WNDCLASSW wc{};
	wc.lpfnWndProc = DefWindowProcW;
	wc.hInstance = GetModuleHandleW(nullptr);
	wc.lpszClassName = L"FerritronClapTestHost";
	RegisterClassW(&wc);
	RECT r{0, 0, static_cast<LONG>(w), static_cast<LONG>(h)};
	AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
	HWND hwnd = CreateWindowW(wc.lpszClassName, L"Ferritron CLAP test host", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 100, 100,
	                          r.right - r.left, r.bottom - r.top, nullptr, nullptr, wc.hInstance, nullptr);
	clap_window_t window{};
	window.api = CLAP_WINDOW_API_WIN32;
	window.win32 = hwnd;
	if (!gui->set_parent(plugin, &window) || !gui->show(plugin))
		return fail("gui set_parent failed");

	std::wstring dir = shotDir;
	pump(2.0);
	screenshot(hwnd, shot(dir, L"clap-1-initial").c_str());

	// The host changes the value through a flush, as it does outside processing.
	EventList in, out;
	clap_event_param_value_t ev{};
	ev.header.size = sizeof(ev);
	ev.header.space_id = CLAP_CORE_EVENT_SPACE_ID;
	ev.header.type = CLAP_EVENT_PARAM_VALUE;
	ev.param_id = info.id;
	ev.note_id = -1;
	ev.port_index = ev.channel = ev.key = -1;
	ev.value = info.min_value + 0.25 * (info.max_value - info.min_value);
	auto* bytes = reinterpret_cast<const uint8_t*>(&ev);
	in.events.emplace_back(bytes, bytes + sizeof(ev));
	params->flush(plugin, &in.in, &out.out);
	std::printf("host: set param %u to %.2f\n", info.id, ev.value);
	pump(0.5);
	screenshot(hwnd, shot(dir, L"clap-2-after-host-change").c_str());

	click(hwnd, static_cast<int>(clickX * scale), static_cast<int>(clickY * scale));
	pump(0.8);
	std::printf("plugin requested flush: %s\n", g_flushRequested ? "yes" : "no");
	EventList none, edits;
	params->flush(plugin, &none.in, &edits.out);
	for (auto& e : edits.events) {
		auto* header = reinterpret_cast<const clap_event_header_t*>(e.data());
		if (header->type == CLAP_EVENT_PARAM_GESTURE_BEGIN)
			std::printf("host: gesture begin\n");
		else if (header->type == CLAP_EVENT_PARAM_GESTURE_END)
			std::printf("host: gesture end\n");
		else if (header->type == CLAP_EVENT_PARAM_VALUE)
			std::printf("host: value %.2f\n", reinterpret_cast<const clap_event_param_value_t*>(header)->value);
	}
	double value = 0;
	params->get_value(plugin, info.id, &value);
	std::printf("plugin: param %u is now %.2f\n", info.id, value);
	screenshot(hwnd, shot(dir, L"clap-3-after-click").c_str());

	if (resizeW > 0 && resizeH > 0) {
		uint32_t rw = static_cast<uint32_t>(resizeW), rh = static_cast<uint32_t>(resizeH);
		gui->adjust_size(plugin, &rw, &rh);
		std::printf("host: resize to %dx%d -> allowed %ux%u\n", resizeW, resizeH, rw, rh);
		setClientSize(hwnd, static_cast<int>(rw), static_cast<int>(rh));
		std::printf("plugin accepted set_size: %s\n", gui->set_size(plugin, rw, rh) ? "yes" : "no");
		pump(0.8);
		printClientSize("resized", hwnd);
		screenshot(hwnd, shot(dir, L"clap-4-resized").c_str());
	}

	gui->destroy(plugin);
	DestroyWindow(hwnd);
	if (resizeW > 0 && resizeH > 0) {
		// The size the user chose travels with the project.
		auto* state = static_cast<const clap_plugin_state_t*>(plugin->get_extension(plugin, CLAP_EXT_STATE));
		static std::vector<uint8_t> saved;
		static size_t readAt;
		saved.clear();
		readAt = 0;
		clap_ostream_t sink{nullptr, [](const clap_ostream_t*, const void* buf, uint64_t n) -> int64_t {
			auto* b = static_cast<const uint8_t*>(buf);
			saved.insert(saved.end(), b, b + n);
			return static_cast<int64_t>(n);
		}};
		clap_istream_t source{nullptr, [](const clap_istream_t*, void* buf, uint64_t n) -> int64_t {
			uint64_t k = std::min<uint64_t>(n, saved.size() - readAt);
			std::memcpy(buf, saved.data() + readAt, k);
			readAt += k;
			return static_cast<int64_t>(k);
		}};
		const clap_plugin_t* other = factory->create_plugin(factory, &host, desc->id);
		if (state && other && other->init(other) && state->save(plugin, &sink)) {
			auto* otherState = static_cast<const clap_plugin_state_t*>(other->get_extension(other, CLAP_EXT_STATE));
			auto* otherGui = static_cast<const clap_plugin_gui_t*>(other->get_extension(other, CLAP_EXT_GUI));
			uint32_t rw = 0, rh = 0;
			if (otherState->load(other, &source) && otherGui->create(other, CLAP_WINDOW_API_WIN32, false) &&
			    otherGui->get_size(other, &rw, &rh))
				std::printf("reloaded editor: %ux%u\n", rw, rh);
			otherGui->destroy(other);
		}
		if (other)
			other->destroy(other);
	}
	plugin->destroy(plugin);
	entry->deinit();
	FreeLibrary(module);
	return 0;
}
