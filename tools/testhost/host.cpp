// Minimal VST3 test host for Ferritron development.
//
// Loads a .vst3 bundle, feeds a constant 1.0 signal through it and prints the
// first output sample whenever it changes. Used to check hot reload without a
// DAW. Compiled against the developer-provided VST3 SDK.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

#include "pluginterfaces/base/ipluginbase.h"
#include "pluginterfaces/base/ibstream.h"
#include "pluginterfaces/vst/ivstaudioprocessor.h"
#include "pluginterfaces/vst/ivstcomponent.h"
#include "pluginterfaces/gui/iplugview.h"
#include "pluginterfaces/gui/iplugviewcontentscalesupport.h"
#include "pluginterfaces/vst/ivsteditcontroller.h"

#include <windows.h>
#include <tlhelp32.h>

#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <algorithm>
#include <string>
#include <thread>
#include <vector>

using namespace Steinberg;
using namespace Steinberg::Vst;

namespace {

template <class T>
T* query(FUnknown* obj, const TUID iid) {
	void* out = nullptr;
	return obj && obj->queryInterface(iid, &out) == kResultOk ? static_cast<T*>(out) : nullptr;
}

int fail(const char* message) {
	std::fprintf(stderr, "testhost: %s\n", message);
	return 1;
}

// Logs edits the plugin reports, e.g. from its editor.
class Handler final : public IComponentHandler {
public:
	tresult PLUGIN_API queryInterface(const TUID, void** obj) override {
		*obj = nullptr;
		return kNoInterface;
	}
	uint32 PLUGIN_API addRef() override { return 1; }
	uint32 PLUGIN_API release() override { return 1; }
	tresult PLUGIN_API beginEdit(ParamID id) override {
		std::printf("host: beginEdit %u\n", id);
		return kResultOk;
	}
	tresult PLUGIN_API performEdit(ParamID id, ParamValue value) override {
		std::printf("host: performEdit %u = %.4f\n", id, value);
		return kResultOk;
	}
	tresult PLUGIN_API endEdit(ParamID id) override {
		std::printf("host: endEdit %u\n", id);
		return kResultOk;
	}
	tresult PLUGIN_API restartComponent(int32) override { return kResultOk; }
};

// Milliseconds since `since`, for the timing lines (`timing: ...`).
double msSince(std::chrono::steady_clock::time_point since) {
	return std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - since).count();
}

void pump(double seconds) {
	auto end = std::chrono::steady_clock::now() + std::chrono::duration<double>(seconds);
	while (std::chrono::steady_clock::now() < end) {
		MSG msg;
		while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) {
			TranslateMessage(&msg);
			DispatchMessageW(&msg);
		}
		std::this_thread::sleep_for(std::chrono::milliseconds(5));
	}
}

// Saves the window's client area (including WebView2 content) as a BMP.
void screenshot(HWND hwnd, const wchar_t* path) {
	RECT rc;
	GetClientRect(hwnd, &rc);
	int w = rc.right, h = rc.bottom;
	// FERRITRON_TESTHOST_SCREEN=1: copy the screen instead (with popups such as
	// context menus, which belong to other windows), 300 pixels beyond the client area.
	const bool fromScreen = std::getenv("FERRITRON_TESTHOST_SCREEN") != nullptr;
	if (fromScreen) {
		w += 300;
		h += 300;
	}
	HDC screen = GetDC(fromScreen ? nullptr : hwnd);
	HDC mem = CreateCompatibleDC(screen);
	HBITMAP bmp = CreateCompatibleBitmap(screen, w, h);
	SelectObject(mem, bmp);
	if (fromScreen) {
		POINT origin{0, 0};
		ClientToScreen(hwnd, &origin);
		BitBlt(mem, 0, 0, w, h, screen, origin.x, origin.y, SRCCOPY);
	} else {
		PrintWindow(hwnd, mem, PW_CLIENTONLY | 0x2 /* PW_RENDERFULLCONTENT */);
	}
	BITMAPINFOHEADER bi{sizeof(bi), w, -h, 1, 24, BI_RGB};
	int stride = ((w * 3 + 3) & ~3);
	std::vector<uint8_t> pixels(static_cast<size_t>(stride) * h);
	GetDIBits(mem, bmp, 0, h, pixels.data(), reinterpret_cast<BITMAPINFO*>(&bi), DIB_RGB_COLORS);
	BITMAPFILEHEADER bf{0x4D42, static_cast<DWORD>(sizeof(bf) + sizeof(bi) + pixels.size()), 0, 0, sizeof(bf) + sizeof(bi)};
	FILE* f = nullptr;
	if (_wfopen_s(&f, path, L"wb") == 0 && f) {
		fwrite(&bf, sizeof(bf), 1, f);
		fwrite(&bi, sizeof(bi), 1, f);
		fwrite(pixels.data(), 1, pixels.size(), f);
		fclose(f);
	}
	DeleteObject(bmp);
	DeleteDC(mem);
	ReleaseDC(fromScreen ? nullptr : hwnd, screen);
	std::wprintf(L"screenshot: %ls\n", path);
}

// Resizes a window so that its client area is width x height.
void setClientSize(HWND hwnd, int width, int height) {
	RECT r{0, 0, width, height};
	AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
	SetWindowPos(hwnd, nullptr, 0, 0, r.right - r.left, r.bottom - r.top, SWP_NOMOVE | SWP_NOZORDER);
}

void printClientSize(const char* label, HWND hwnd) {
	RECT rc;
	GetClientRect(hwnd, &rc);
	std::printf("%s: window %ldx%ld\n", label, rc.right, rc.bottom);
}

// Lets the plugin resize the host window (IPlugFrame::resizeView).
class Frame final : public IPlugFrame {
public:
	HWND hwnd = nullptr;
	tresult PLUGIN_API queryInterface(const TUID, void** obj) override {
		*obj = nullptr;
		return kNoInterface;
	}
	uint32 PLUGIN_API addRef() override { return 1; }
	uint32 PLUGIN_API release() override { return 1; }
	tresult PLUGIN_API resizeView(IPlugView* view, ViewRect* size) override {
		std::printf("host: plugin asked for %dx%d\n", size->getWidth(), size->getHeight());
		if (hwnd)
			setClientSize(hwnd, size->getWidth(), size->getHeight());
		return view->onSize(size);
	}
};

// FERRITRON_TESTHOST_POST_CLICK=1: post the click to the window under the
// point (also in another process) instead of moving the real mouse.
void click(HWND hwnd, int x, int y) {
	if (std::getenv("FERRITRON_TESTHOST_POST_CLICK")) {
		HWND target = hwnd;
		POINT at{x, y};
		for (;;) {
			HWND child = ChildWindowFromPointEx(target, at, CWP_SKIPINVISIBLE | CWP_SKIPTRANSPARENT);
			if (!child || child == target)
				break;
			MapWindowPoints(target, child, &at, 1);
			target = child;
		}
		LPARAM pos = MAKELPARAM(at.x, at.y);
		// FERRITRON_TESTHOST_RIGHT_CLICK=1: the right button (context menus).
		if (std::getenv("FERRITRON_TESTHOST_RIGHT_CLICK")) {
			PostMessageW(target, WM_RBUTTONDOWN, MK_RBUTTON, pos);
			pump(0.15);
			PostMessageW(target, WM_RBUTTONUP, 0, pos);
			return;
		}
		PostMessageW(target, WM_LBUTTONDOWN, MK_LBUTTON, pos);
		pump(0.15);
		PostMessageW(target, WM_LBUTTONUP, 0, pos);
		return;
	}
	POINT p{x, y};
	ClientToScreen(hwnd, &p);
	SetForegroundWindow(hwnd);
	SetCursorPos(p.x, p.y);
	INPUT in[2] = {};
	in[0].type = in[1].type = INPUT_MOUSE;
	in[0].mi.dwFlags = MOUSEEVENTF_LEFTDOWN;
	in[1].mi.dwFlags = MOUSEEVENTF_LEFTUP;
	SendInput(1, &in[0], sizeof(INPUT));
	pump(0.15);
	SendInput(1, &in[1], sizeof(INPUT));
}

// Keys that reach the host's own window (the editor passes on keys it does not use).
LRESULT CALLBACK hostProc(HWND hwnd, UINT msg, WPARAM w, LPARAM l) {
	if (msg == WM_KEYDOWN)
		std::printf("host window: key down 0x%02X\n", static_cast<unsigned>(w));
	return DefWindowProcW(hwnd, msg, w, l);
}

// FERRITRON_TESTHOST_KEYS="26,26,t:12,0D": real keyboard input (SendInput) to
// whatever has the focus, after the click: a hex virtual-key code is pressed
// and released, "t:TEXT" types the characters.
void sendKey(WORD vk, DWORD flags, WORD scan = 0) {
	INPUT in{};
	in.type = INPUT_KEYBOARD;
	in.ki.wVk = vk;
	in.ki.wScan = scan;
	in.ki.dwFlags = flags;
	SendInput(1, &in, sizeof(INPUT));
}

void sendKeys(HWND hwnd, const char* spec) {
	SetForegroundWindow(hwnd);
	std::string all = spec;
	size_t at = 0;
	while (at <= all.size()) {
		size_t end = all.find(',', at);
		if (end == std::string::npos)
			end = all.size();
		std::string token = all.substr(at, end - at);
		at = end + 1;
		if (token.rfind("t:", 0) == 0) {
			for (char c : token.substr(2)) {
				sendKey(0, KEYEVENTF_UNICODE, static_cast<WORD>(c));
				sendKey(0, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP, static_cast<WORD>(c));
				pump(0.05);
			}
		} else if (!token.empty()) {
			WORD vk = static_cast<WORD>(std::strtoul(token.c_str(), nullptr, 16));
			sendKey(vk, 0);
			pump(0.05);
			sendKey(vk, KEYEVENTF_KEYUP);
			pump(0.05);
		}
	}
}

// Child processes of this one running rundll32 (isolated plugins).
int countChildProcesses() {
	int count = 0;
	HANDLE snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
	PROCESSENTRY32W pe{sizeof(pe)};
	for (BOOL ok = Process32FirstW(snap, &pe); ok; ok = Process32NextW(snap, &pe))
		if (pe.th32ParentProcessID == GetCurrentProcessId() && _wcsicmp(pe.szExeFile, L"rundll32.exe") == 0)
			++count;
	CloseHandle(snap);
	return count;
}

// A growable in-memory IBStream, for saving and loading state.
class MemoryStream final : public IBStream {
public:
	std::vector<uint8_t> data;
	size_t at = 0;
	tresult PLUGIN_API queryInterface(const TUID, void** obj) override {
		*obj = nullptr;
		return kNoInterface;
	}
	uint32 PLUGIN_API addRef() override { return 1; }
	uint32 PLUGIN_API release() override { return 1; }
	tresult PLUGIN_API read(void* buffer, int32 n, int32* done) override {
		int32 k = static_cast<int32>(std::min<size_t>(n, data.size() - at));
		std::memcpy(buffer, data.data() + at, k);
		at += k;
		if (done)
			*done = k;
		return kResultOk;
	}
	tresult PLUGIN_API write(void* buffer, int32 n, int32* done) override {
		auto* bytes = static_cast<uint8_t*>(buffer);
		data.insert(data.end(), bytes, bytes + n);
		if (done)
			*done = n;
		return kResultOk;
	}
	tresult PLUGIN_API seek(int64 pos, int32 mode, int64* result) override {
		at = mode == IBStream::kIBSeekSet ? pos : mode == IBStream::kIBSeekCur ? at + pos : data.size() + pos;
		if (result)
			*result = at;
		return kResultOk;
	}
	tresult PLUGIN_API tell(int64* pos) override {
		*pos = at;
		return kResultOk;
	}
};

// Opens the plugin editor in a window and exercises both sync directions.
int runGui(IEditController* controller, const wchar_t* shotDir, int clickX, int clickY, double scale, int resizeW,
           int resizeH) {
	OleInitialize(nullptr);
	// FERRITRON_TESTHOST_OPEN_AFTER=S: the host runs for S seconds before the
	// user opens the editor (time a plugin may use to get ready).
	if (const char* after = std::getenv("FERRITRON_TESTHOST_OPEN_AFTER"))
		pump(std::atof(after));
	IPlugView* view = controller->createView(ViewType::kEditor);
	if (!view)
		return fail("the plugin has no editor");
	static Frame frame;
	view->setFrame(&frame);
	if (scale > 0) {
		void* obj = nullptr;
		if (view->queryInterface(IPlugViewContentScaleSupport_iid, &obj) == kResultOk) {
			auto* support = static_cast<IPlugViewContentScaleSupport*>(obj);
			support->setContentScaleFactor(static_cast<float>(scale));
			support->release();
			std::printf("host: content scale %.2f\n", scale);
		}
	} else {
		scale = 1.0;
	}
	ViewRect size{};
	view->getSize(&size);
	std::printf("editor: %dx%d (resizable: %s)\n", size.getWidth(), size.getHeight(),
	            view->canResize() == kResultTrue ? "yes" : "no");

	WNDCLASSW wc{};
	wc.lpfnWndProc = hostProc;
	wc.hInstance = GetModuleHandleW(nullptr);
	wc.lpszClassName = L"FerritronTestHost";
	RegisterClassW(&wc);
	RECT r{0, 0, size.getWidth(), size.getHeight()};
	AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
	HWND hwnd = CreateWindowW(wc.lpszClassName, L"Ferritron test host", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 100, 100,
	                          r.right - r.left, r.bottom - r.top, nullptr, nullptr, wc.hInstance, nullptr);
	frame.hwnd = hwnd;
	auto attachStart = std::chrono::steady_clock::now();
	if (view->attached(hwnd, kPlatformTypeHWND) != kResultOk)
		return fail("attaching the editor failed");
	std::printf("timing: attached in %.1f ms\n", msSince(attachStart));
	printClientSize("attached", hwnd);

	std::wstring dir = shotDir;
	ParameterInfo param{};
	controller->getParameterInfo(0, param);
	pump(2.0);
	screenshot(hwnd, (dir + L"\\gui-1-initial.bmp").c_str());

	controller->setParamNormalized(param.id, 0.25); // as automation would
	std::printf("host: set param %u to 0.25\n", param.id);
	pump(0.5);
	screenshot(hwnd, (dir + L"\\gui-2-after-host-change.bmp").c_str());

	click(hwnd, static_cast<int>(clickX * scale), static_cast<int>(clickY * scale));
	pump(0.8);
	std::printf("plugin: param %u is now %.4f\n", param.id, controller->getParamNormalized(param.id));
	screenshot(hwnd, (dir + L"\\gui-3-after-click.bmp").c_str());

	if (const char* keys = std::getenv("FERRITRON_TESTHOST_KEYS")) {
		sendKeys(hwnd, keys);
		pump(0.8);
		std::printf("plugin: after keys \"%s\", param %u is now %.4f\n", keys, param.id,
		            controller->getParamNormalized(param.id));
		screenshot(hwnd, (dir + L"\\gui-3b-after-keys.bmp").c_str());
	}

	if (resizeW > 0 && resizeH > 0) {
		ViewRect want(0, 0, resizeW, resizeH);
		view->checkSizeConstraint(&want);
		std::printf("host: resize to %dx%d -> allowed %dx%d\n", resizeW, resizeH, want.getWidth(), want.getHeight());
		setClientSize(hwnd, want.getWidth(), want.getHeight());
		view->onSize(&want);
		pump(0.8);
		printClientSize("resized", hwnd);
		screenshot(hwnd, (dir + L"\\gui-4-resized.bmp").c_str());
	}

	// FERRITRON_TESTHOST_REOPEN=N: close and reopen the editor N times, as a
	// user does, timing each open.
	const char* reopen = std::getenv("FERRITRON_TESTHOST_REOPEN");
	for (int i = 0; reopen && i < std::atoi(reopen); ++i) {
		view->removed();
		view->release();
		pump(0.3);
		view = controller->createView(ViewType::kEditor);
		if (!view)
			return fail("the plugin has no editor on reopen");
		view->setFrame(&frame);
		auto again = std::chrono::steady_clock::now();
		if (view->attached(hwnd, kPlatformTypeHWND) != kResultOk)
			return fail("reattaching the editor failed");
		std::printf("timing: reopened in %.1f ms\n", msSince(again));
		pump(0.5);
	}
	if (reopen)
		screenshot(hwnd, (dir + L"\\gui-5-reopened.bmp").c_str());

	view->removed();
	view->release();
	DestroyWindow(hwnd);
	return 0;
}

} // namespace

extern "C" int ferritron_testhost_run(const wchar_t* binary, double seconds, double gainNormalized,
                                      const wchar_t* guiShotDir, int clickX, int clickY, double scale, int resizeW,
                                      int resizeH, double pulseAt) {
	// As DAWs do: the UI thread is a single-threaded COM apartment from the start.
	OleInitialize(nullptr);
	auto loadStart = std::chrono::steady_clock::now();
	HMODULE module = LoadLibraryW(binary);
	if (!module)
		return fail("cannot load the plugin binary");
	std::printf("timing: LoadLibrary %.1f ms\n", msSince(loadStart));
	using InitDll = bool (*)();
	using GetFactory = IPluginFactory* (*)();
	if (auto init = reinterpret_cast<InitDll>(GetProcAddress(module, "InitDll")))
		init();
	auto getFactory = reinterpret_cast<GetFactory>(GetProcAddress(module, "GetPluginFactory"));
	IPluginFactory* factory = getFactory ? getFactory() : nullptr;
	if (!factory || factory->countClasses() < 1)
		return fail("no plugin factory");

	PClassInfo info{};
	factory->getClassInfo(0, &info);
	std::printf("plugin: %s\n", info.name);

	void* obj = nullptr;
	auto createStart = std::chrono::steady_clock::now();
	if (factory->createInstance(info.cid, IComponent_iid, &obj) != kResultOk)
		return fail("createInstance failed");
	auto* component = static_cast<IComponent*>(obj);
	component->initialize(nullptr);
	std::printf("timing: createInstance + initialize %.1f ms (%.1f ms since LoadLibrary)\n", msSince(createStart),
	            msSince(loadStart));
	auto* processor = query<IAudioProcessor>(component, IAudioProcessor_iid);
	auto* controller = query<IEditController>(component, IEditController_iid);
	if (!processor || !controller)
		return fail("missing IAudioProcessor or IEditController");
	static Handler handler;
	controller->setComponentHandler(&handler);

	// FERRITRON_TESTHOST_PARAM=N: --param and --pulse-param-at act on parameter N (default 0).
	const char* chosen = std::getenv("FERRITRON_TESTHOST_PARAM");
	const int32 target = chosen ? std::atoi(chosen) : 0;
	for (int32 i = 0, n = controller->getParameterCount(); i < n; ++i) {
		ParameterInfo param{};
		controller->getParameterInfo(i, param);
		if (i == target && gainNormalized >= 0)
			controller->setParamNormalized(param.id, gainNormalized);
		String128 text{};
		controller->getParamStringByValue(param.id, controller->getParamNormalized(param.id), text);
		char title[512], value[512], units[512];
		WideCharToMultiByte(CP_UTF8, 0, reinterpret_cast<const wchar_t*>(param.title), -1, title, sizeof(title), nullptr, nullptr);
		WideCharToMultiByte(CP_UTF8, 0, reinterpret_cast<const wchar_t*>(text), -1, value, sizeof(value), nullptr, nullptr);
		WideCharToMultiByte(CP_UTF8, 0, reinterpret_cast<const wchar_t*>(param.units), -1, units, sizeof(units), nullptr, nullptr);
		std::printf("param %d: %s = %s %s\n", i, title, value, units);
	}

	if (guiShotDir) {
		int code = runGui(controller, guiShotDir, clickX, clickY, scale, resizeW, resizeH);
		if (code == 0 && resizeW > 0 && resizeH > 0) {
			// The size the user chose travels with the project: a new instance
			// loading the saved state opens its editor at that size.
			MemoryStream state;
			component->getState(&state);
			void* other = nullptr;
			factory->createInstance(info.cid, IComponent_iid, &other);
			auto* reloaded = static_cast<IComponent*>(other);
			reloaded->initialize(nullptr);
			state.at = 0;
			reloaded->setState(&state);
			auto* reloadedController = query<IEditController>(reloaded, IEditController_iid);
			if (IPlugView* view = reloadedController ? reloadedController->createView(ViewType::kEditor) : nullptr) {
				ViewRect size{};
				view->getSize(&size);
				std::printf("reloaded editor: %dx%d\n", size.getWidth(), size.getHeight());
				view->release();
			}
			if (reloadedController)
				reloadedController->release();
			reloaded->terminate();
			reloaded->release();
		}
		controller->setComponentHandler(nullptr);
		controller->release();
		processor->release();
		component->terminate();
		component->release();
		return code;
	}

	constexpr int32 frames = 256;
	// FERRITRON_TESTHOST_OFFLINE=1: render like a bounce (processMode kOffline).
	const bool offline = std::getenv("FERRITRON_TESTHOST_OFFLINE") != nullptr;
	ProcessSetup setup{offline ? kOffline : kRealtime, kSample32, frames, 48000.0};
	auto activateStart = std::chrono::steady_clock::now();
	processor->setupProcessing(setup);
	component->setActive(true);
	std::printf("timing: setupProcessing + setActive %.1f ms\n", msSince(activateStart));
	std::printf("latency: %u samples\n", processor->getLatencySamples());
	processor->setProcessing(true);

	// FERRITRON_TESTHOST_INSTANCES=N: N instances process side by side
	// (the extra ones with their own buffers).
	struct Extra {
		IComponent* component;
		IAudioProcessor* processor;
		std::vector<float> l, r;
	};
	std::vector<Extra> extras;
	const char* instances = std::getenv("FERRITRON_TESTHOST_INSTANCES");
	for (int i = 1; instances && i < std::atoi(instances); ++i) {
		void* more = nullptr;
		if (factory->createInstance(info.cid, IComponent_iid, &more) != kResultOk)
			return fail("createInstance failed");
		auto* c = static_cast<IComponent*>(more);
		c->initialize(nullptr);
		auto* p = query<IAudioProcessor>(c, IAudioProcessor_iid);
		p->setupProcessing(setup);
		c->setActive(true);
		p->setProcessing(true);
		extras.push_back({c, p, std::vector<float>(frames, 1.0f), std::vector<float>(frames, 1.0f)});
	}
	if (instances)
		std::printf("instances: %zu, isolation processes: %d\n", extras.size() + 1, countChildProcesses());

	std::vector<float> inL(frames), inR(frames), outL(frames), outR(frames);
	float* ins[2] = {inL.data(), inR.data()};
	float* outs[2] = {outL.data(), outR.data()};
	AudioBusBuffers input{};
	input.numChannels = 2;
	input.channelBuffers32 = ins;
	AudioBusBuffers output{};
	output.numChannels = 2;
	output.channelBuffers32 = outs;
	ProcessData data{};
	data.processMode = kRealtime;
	data.symbolicSampleSize = kSample32;
	data.numSamples = frames;
	data.numInputs = 1;
	data.numOutputs = 1;
	data.inputs = &input;
	data.outputs = &output;

	float last = NAN;
	auto start = std::chrono::steady_clock::now();
	ParamID pulseId = 0;
	if (pulseAt >= 0) {
		ParameterInfo first{};
		controller->getParameterInfo(target, first);
		pulseId = first.id;
	}
	bool pulsed = false;
	for (;;) {
		double now = std::chrono::duration<double>(std::chrono::steady_clock::now() - start).count();
		if (pulseAt >= 0 && !pulsed && now >= pulseAt) {
			// Parameter 0 on for one block, then off again.
			controller->setParamNormalized(pulseId, 1.0);
			processor->process(data);
			controller->setParamNormalized(pulseId, 0.0);
			pulsed = true;
			std::printf("[%6.2fs] host: pulsed parameter 0\n", now);
		}
		std::fill(inL.begin(), inL.end(), 1.0f);
		std::fill(inR.begin(), inR.end(), 1.0f);
		processor->process(data);
		for (auto& e : extras) {
			std::fill(e.l.begin(), e.l.end(), 1.0f);
			std::fill(e.r.begin(), e.r.end(), 1.0f);
			float* io[2] = {e.l.data(), e.r.data()};
			AudioBusBuffers bus{};
			bus.numChannels = 2;
			bus.channelBuffers32 = io;
			ProcessData d = data;
			d.inputs = &bus;
			d.outputs = &bus;
			e.processor->process(d);
		}
		double t = std::chrono::duration<double>(std::chrono::steady_clock::now() - start).count();
		if (outL[0] != last) {
			last = outL[0];
			std::printf("[%6.2fs] output = %.6f\n", t, last);
			std::fflush(stdout);
		}
		if (t >= seconds)
			break;
		std::this_thread::sleep_for(std::chrono::milliseconds(5)); // ~ one block per 5 ms
	}

	for (auto& e : extras) {
		e.processor->setProcessing(false);
		e.component->setActive(false);
		e.processor->release();
		e.component->terminate();
		e.component->release();
	}
	processor->setProcessing(false);
	component->setActive(false);
	controller->release();
	processor->release();
	component->terminate();
	component->release();
	factory->release();
	if (auto exit = reinterpret_cast<InitDll>(GetProcAddress(module, "ExitDll")))
		exit();
	FreeLibrary(module);
	if (instances)
		std::printf("isolation processes left: %d\n", countChildProcesses());
	std::printf("done\n");
	return 0;
}

#ifdef FERRITRON_HAVE_CLAP
#include "clap_checks.inc"
#endif

#include "shared_runtime.inc"
