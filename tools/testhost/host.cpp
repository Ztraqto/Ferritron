// Minimal VST3 test host for Ferritron development.
//
// Loads a .vst3 bundle, feeds a constant 1.0 signal through it and prints the
// first output sample whenever it changes. Used to check hot reload without a
// DAW. Compiled against the developer-provided VST3 SDK.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

#include "pluginterfaces/base/ipluginbase.h"
#include "pluginterfaces/base/ibstream.h"
#include "pluginterfaces/base/iplugincompatibility.h"
#include "pluginterfaces/vst/ivstaudioprocessor.h"
#include "pluginterfaces/vst/ivstprocesscontext.h"
#include "pluginterfaces/vst/ivstparameterchanges.h"
#include "pluginterfaces/vst/ivstevents.h"
#include "pluginterfaces/vst/ivstcomponent.h"
#include "pluginterfaces/gui/iplugview.h"
#include "pluginterfaces/gui/iplugviewcontentscalesupport.h"
#include "pluginterfaces/vst/ivsteditcontroller.h"
#include "pluginterfaces/vst/ivstattributes.h"
#include "pluginterfaces/vst/ivstremapparamid.h"
#include "pluginterfaces/vst/ivstchannelcontextinfo.h"

#include <windows.h>
#include <tlhelp32.h>

#ifdef FERRITRON_HAVE_VST2
// Your own copy of the VST 2 SDK (VST2_SDK_DIR); Ferritron neither ships nor fetches it.
#define VST_FORCE_DEPRECATED 0
#include "pluginterfaces/vst2.x/aeffectx.h"
#endif

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
// FERRITRON_TESTHOST_DENSE=N: every block carries N automation points for the
// target parameter (offsets spread over the block, values zig-zagging; the last is 1.0).
struct DenseQueue : IParamValueQueue {
	ParamID id = 0;
	int32 count = 0, frames = 1;
	tresult PLUGIN_API queryInterface(const TUID, void**) override { return kNoInterface; }
	uint32 PLUGIN_API addRef() override { return 1; }
	uint32 PLUGIN_API release() override { return 1; }
	ParamID PLUGIN_API getParameterId() override { return id; }
	int32 PLUGIN_API getPointCount() override { return count; }
	tresult PLUGIN_API getPoint(int32 i, int32& offset, ParamValue& value) override {
		if (i < 0 || i >= count)
			return kResultFalse;
		offset = static_cast<int32>(static_cast<int64>(i) * frames / count);
		value = i == count - 1 ? 1.0 : (i % 2 ? 0.25 : 0.75);
		return kResultOk;
	}
	tresult PLUGIN_API addPoint(int32, ParamValue, int32&) override { return kResultFalse; }
};
struct DenseChanges : IParameterChanges {
	DenseQueue queue;
	tresult PLUGIN_API queryInterface(const TUID, void**) override { return kNoInterface; }
	uint32 PLUGIN_API addRef() override { return 1; }
	uint32 PLUGIN_API release() override { return 1; }
	int32 PLUGIN_API getParameterCount() override { return 1; }
	IParamValueQueue* PLUGIN_API getParameterData(int32 i) override { return i == 0 ? &queue : nullptr; }
	IParamValueQueue* PLUGIN_API addParameterData(const ParamID&, int32&) override { return nullptr; }
};
// What the plugin sends back to the host (FERRITRON_TESTHOST_OUT=1 prints each block's count).
struct OutQueue : IParamValueQueue {
	ParamID id = 0;
	int32 points = 0;
	tresult PLUGIN_API queryInterface(const TUID, void**) override { return kNoInterface; }
	uint32 PLUGIN_API addRef() override { return 1; }
	uint32 PLUGIN_API release() override { return 1; }
	ParamID PLUGIN_API getParameterId() override { return id; }
	int32 PLUGIN_API getPointCount() override { return points; }
	tresult PLUGIN_API getPoint(int32, int32&, ParamValue&) override { return kResultFalse; }
	tresult PLUGIN_API addPoint(int32 offset, ParamValue value, int32& index) override {
		std::printf("host: output parameter %u = %.3f at %d\n", id, value, offset);
		index = points++;
		return kResultOk;
	}
};
struct OutChanges : IParameterChanges {
	OutQueue queue;
	tresult PLUGIN_API queryInterface(const TUID, void**) override { return kNoInterface; }
	uint32 PLUGIN_API addRef() override { return 1; }
	uint32 PLUGIN_API release() override { return 1; }
	int32 PLUGIN_API getParameterCount() override { return 0; }
	IParamValueQueue* PLUGIN_API getParameterData(int32) override { return nullptr; }
	IParamValueQueue* PLUGIN_API addParameterData(const ParamID& id, int32& index) override {
		queue.id = id;
		index = 0;
		return &queue;
	}
};
struct OutEvents : IEventList {
	int32 count = 0;
	tresult PLUGIN_API queryInterface(const TUID, void**) override { return kNoInterface; }
	uint32 PLUGIN_API addRef() override { return 1; }
	uint32 PLUGIN_API release() override { return 1; }
	int32 PLUGIN_API getEventCount() override { return count; }
	tresult PLUGIN_API getEvent(int32, Event&) override { return kResultFalse; }
	tresult PLUGIN_API addEvent(Event& e) override {
		if (e.type == Event::kLegacyMIDICCOutEvent)
			std::printf("host: output MIDI controller %d = %d at %d\n", e.midiCCOut.controlNumber, e.midiCCOut.value, e.sampleOffset);
		else
			std::printf("host: output event type %d at %d\n", e.type, e.sampleOffset);
		++count;
		return kResultOk;
	}
};
} // namespace

namespace {

// The process environment as it is now: unlike std::getenv (the C runtime's
// copy from startup), this sees variables the Rust side set (--instances, ...).
const char* envVar(const char* name) {
	static thread_local char value[4096];
	DWORD n = GetEnvironmentVariableA(name, value, sizeof(value));
	return n > 0 && n < sizeof(value) ? value : nullptr;
}

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

// --- PNG (uncompressed deflate: no library needed) ---------------------------

uint32_t crc32(const uint8_t* data, size_t len, uint32_t crc = 0) {
	static uint32_t table[256];
	if (!table[1])
		for (uint32_t n = 0; n < 256; ++n) {
			uint32_t c = n;
			for (int k = 0; k < 8; ++k)
				c = (c & 1) ? 0xEDB88320u ^ (c >> 1) : c >> 1;
			table[n] = c;
		}
	crc = ~crc;
	for (size_t i = 0; i < len; ++i)
		crc = table[(crc ^ data[i]) & 0xFF] ^ (crc >> 8);
	return ~crc;
}

void putBe32(std::vector<uint8_t>& out, uint32_t v) {
	for (int shift = 24; shift >= 0; shift -= 8)
		out.push_back(static_cast<uint8_t>(v >> shift));
}

void pngChunk(std::vector<uint8_t>& out, const char* type, const std::vector<uint8_t>& body) {
	putBe32(out, static_cast<uint32_t>(body.size()));
	size_t start = out.size();
	out.insert(out.end(), type, type + 4);
	out.insert(out.end(), body.begin(), body.end());
	putBe32(out, crc32(out.data() + start, out.size() - start));
}

// `bgr`: bottom-up = false, rows of `stride` bytes, 3 bytes per pixel (B, G, R).
std::vector<uint8_t> encodePng(int w, int h, const std::vector<uint8_t>& bgr, int stride) {
	std::vector<uint8_t> raw;
	raw.reserve(static_cast<size_t>(h) * (w * 3 + 1));
	for (int y = 0; y < h; ++y) {
		raw.push_back(0); // filter: none
		const uint8_t* row = bgr.data() + static_cast<size_t>(y) * stride;
		for (int x = 0; x < w; ++x) {
			raw.push_back(row[x * 3 + 2]);
			raw.push_back(row[x * 3 + 1]);
			raw.push_back(row[x * 3 + 0]);
		}
	}
	std::vector<uint8_t> z{0x78, 0x01};
	for (size_t at = 0; at < raw.size() || at == 0;) {
		size_t len = std::min<size_t>(65535, raw.size() - at);
		bool last = at + len >= raw.size();
		z.push_back(last ? 1 : 0);
		z.push_back(static_cast<uint8_t>(len));
		z.push_back(static_cast<uint8_t>(len >> 8));
		z.push_back(static_cast<uint8_t>(~len));
		z.push_back(static_cast<uint8_t>(~len >> 8));
		z.insert(z.end(), raw.begin() + at, raw.begin() + at + len);
		at += len;
		if (last)
			break;
	}
	uint32_t a = 1, b = 0;
	for (uint8_t v : raw) {
		a = (a + v) % 65521;
		b = (b + a) % 65521;
	}
	putBe32(z, (b << 16) | a);
	std::vector<uint8_t> png{0x89, 'P', 'N', 'G', '\r', '\n', 0x1A, '\n'};
	std::vector<uint8_t> ihdr;
	putBe32(ihdr, static_cast<uint32_t>(w));
	putBe32(ihdr, static_cast<uint32_t>(h));
	ihdr.insert(ihdr.end(), {8, 2, 0, 0, 0}); // 8-bit RGB
	pngChunk(png, "IHDR", ihdr);
	pngChunk(png, "IDAT", z);
	pngChunk(png, "IEND", {});
	return png;
}

// Screenshot file names: PNG, or BMP with FERRITRON_TESTHOST_BMP=1.
std::wstring shot(const std::wstring& dir, const wchar_t* name) {
	return dir + L"\\" + name + (envVar("FERRITRON_TESTHOST_BMP") ? L".bmp" : L".png");
}

// Saves the window's client area (including WebView2 content) as PNG or BMP (by the path's extension).
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
	const bool bmpFile = std::wcslen(path) > 4 && _wcsicmp(path + std::wcslen(path) - 4, L".bmp") == 0;
	FILE* f = nullptr;
	if (_wfopen_s(&f, path, L"wb") == 0 && f) {
		if (bmpFile) {
			fwrite(&bf, sizeof(bf), 1, f);
			fwrite(&bi, sizeof(bi), 1, f);
			fwrite(pixels.data(), 1, pixels.size(), f);
		} else {
			std::vector<uint8_t> png = encodePng(w, h, pixels, stride);
			fwrite(png.data(), 1, png.size(), f);
		}
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

// The deepest window under client point `at` of `hwnd`; `at` becomes its coordinates.
HWND windowAt(HWND hwnd, POINT& at) {
	HWND target = hwnd;
	for (;;) {
		HWND child = ChildWindowFromPointEx(target, at, CWP_SKIPINVISIBLE | CWP_SKIPTRANSPARENT);
		if (!child || child == target)
			return target;
		MapWindowPoints(target, child, &at, 1);
		target = child;
	}
}

// FERRITRON_TESTHOST_SCRIPT="drag:X0,Y0,X1,Y1;wheel:X,Y,NOTCHES;move:X,Y;leave":
// mouse input posted to the window under each point, in logical pixels
// (times the content scale), after the click and keys.
void runScript(HWND hwnd, const char* script, double scale) {
	std::string all = script;
	size_t at = 0;
	HWND last = hwnd;
	while (at <= all.size()) {
		size_t end = all.find(';', at);
		if (end == std::string::npos)
			end = all.size();
		std::string step = all.substr(at, end - at);
		at = end + 1;
		std::string verb = step.substr(0, step.find(':'));
		std::vector<double> n;
		if (step.find(':') != std::string::npos) {
			std::string args = step.substr(step.find(':') + 1);
			for (size_t p = 0; p <= args.size();) {
				size_t q = args.find(',', p);
				if (q == std::string::npos)
					q = args.size();
				n.push_back(std::atof(args.substr(p, q - p).c_str()));
				p = q + 1;
			}
		}
		auto point = [&](double x, double y) { return POINT{static_cast<LONG>(x * scale), static_cast<LONG>(y * scale)}; };
		if (verb == "drag" && n.size() == 4) {
			POINT from = point(n[0], n[1]);
			HWND target = windowAt(hwnd, from);
			POINT offset{from.x - static_cast<LONG>(n[0] * scale), from.y - static_cast<LONG>(n[1] * scale)};
			PostMessageW(target, WM_MOUSEMOVE, 0, MAKELPARAM(from.x, from.y));
			PostMessageW(target, WM_LBUTTONDOWN, MK_LBUTTON, MAKELPARAM(from.x, from.y));
			pump(0.05);
			for (int i = 1; i <= 10; ++i) {
				double t = i / 10.0;
				POINT p = point(n[0] + (n[2] - n[0]) * t, n[1] + (n[3] - n[1]) * t);
				PostMessageW(target, WM_MOUSEMOVE, MK_LBUTTON, MAKELPARAM(p.x + offset.x, p.y + offset.y));
				pump(0.03);
			}
			POINT to = point(n[2], n[3]);
			PostMessageW(target, WM_LBUTTONUP, 0, MAKELPARAM(to.x + offset.x, to.y + offset.y));
			last = target;
			std::printf("script: drag %.0f,%.0f -> %.0f,%.0f\n", n[0], n[1], n[2], n[3]);
		} else if (verb == "move" && n.size() == 2) {
			POINT p = point(n[0], n[1]);
			last = windowAt(hwnd, p);
			PostMessageW(last, WM_MOUSEMOVE, 0, MAKELPARAM(p.x, p.y));
			std::printf("script: move %.0f,%.0f\n", n[0], n[1]);
		} else if (verb == "wheel" && n.size() == 3) {
			POINT p = point(n[0], n[1]);
			POINT screenAt = p;
			ClientToScreen(hwnd, &screenAt);
			last = windowAt(hwnd, p);
			short delta = static_cast<short>(n[2] * WHEEL_DELTA);
			PostMessageW(last, WM_MOUSEWHEEL, MAKEWPARAM(0, delta), MAKELPARAM(screenAt.x, screenAt.y));
			std::printf("script: wheel %.0f,%.0f by %.1f\n", n[0], n[1], n[2]);
		} else if (verb == "leave") {
			PostMessageW(last, WM_MOUSELEAVE, 0, 0);
			std::printf("script: leave\n");
		} else if (!step.empty()) {
			std::printf("script: unknown step \"%s\"\n", step.c_str());
		}
		pump(0.2);
	}
}

// The track an instance sits on, as a DAW tells it (ChannelContext::IInfoListener).
class TrackAttributes final : public IAttributeList {
public:
	std::u16string name;
	int64 color = 0;
	int64 index = 0;
	tresult PLUGIN_API queryInterface(const TUID, void** obj) override {
		*obj = nullptr;
		return kNoInterface;
	}
	uint32 PLUGIN_API addRef() override { return 1; }
	uint32 PLUGIN_API release() override { return 1; }
	tresult PLUGIN_API setInt(AttrID, int64) override { return kResultFalse; }
	tresult PLUGIN_API getInt(AttrID id, int64& value) override {
		if (std::strcmp(id, ChannelContext::kChannelColorKey) == 0) {
			value = color;
			return kResultOk;
		}
		if (std::strcmp(id, ChannelContext::kChannelIndexKey) == 0) {
			value = index;
			return kResultOk;
		}
		return kResultFalse;
	}
	tresult PLUGIN_API setFloat(AttrID, double) override { return kResultFalse; }
	tresult PLUGIN_API getFloat(AttrID, double&) override { return kResultFalse; }
	tresult PLUGIN_API setString(AttrID, const TChar*) override { return kResultFalse; }
	tresult PLUGIN_API getString(AttrID id, TChar* string, uint32 sizeInBytes) override {
		if (std::strcmp(id, ChannelContext::kChannelNameKey) != 0 || sizeInBytes < 2)
			return kResultFalse;
		size_t chars = std::min<size_t>(name.size(), sizeInBytes / 2 - 1);
		std::memcpy(string, name.data(), chars * 2);
		string[chars] = 0;
		return kResultOk;
	}
	tresult PLUGIN_API setBinary(AttrID, const void*, uint32) override { return kResultFalse; }
	tresult PLUGIN_API getBinary(AttrID, const void*&, uint32&) override { return kResultFalse; }
};

// Tells instance `obj` it sits on track `index` (from 0): "Track N" with a color of its own.
void announceTrack(FUnknown* obj, int index) {
	auto* listener = query<ChannelContext::IInfoListener>(obj, ChannelContext::IInfoListener_iid);
	if (!listener)
		return;
	static const uint32 colors[] = {0xFFE0505A, 0xFF50A0E0, 0xFF60C060, 0xFFE0B040, 0xFFA070E0, 0xFF40C0B0};
	TrackAttributes track;
	std::string name = "Track " + std::to_string(index + 1);
	track.name.assign(name.begin(), name.end());
	track.color = colors[index % 6];
	track.index = index + 1; // VST3 counts from 1
	listener->setChannelContextInfos(&track);
	listener->release();
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
	screenshot(hwnd, shot(dir, L"gui-1-initial").c_str());

	controller->setParamNormalized(param.id, 0.25); // as automation would
	std::printf("host: set param %u to 0.25\n", param.id);
	pump(0.5);
	screenshot(hwnd, shot(dir, L"gui-2-after-host-change").c_str());

	click(hwnd, static_cast<int>(clickX * scale), static_cast<int>(clickY * scale));
	pump(0.8);
	std::printf("plugin: param %u is now %.4f\n", param.id, controller->getParamNormalized(param.id));
	screenshot(hwnd, shot(dir, L"gui-3-after-click").c_str());

	if (const char* keys = std::getenv("FERRITRON_TESTHOST_KEYS")) {
		sendKeys(hwnd, keys);
		pump(0.8);
		std::printf("plugin: after keys \"%s\", param %u is now %.4f\n", keys, param.id,
		            controller->getParamNormalized(param.id));
		screenshot(hwnd, shot(dir, L"gui-3b-after-keys").c_str());
	}

	if (const char* script = envVar("FERRITRON_TESTHOST_SCRIPT")) {
		runScript(hwnd, script, scale);
		pump(0.5);
		std::printf("plugin: after the script, param %u is now %.4f\n", param.id, controller->getParamNormalized(param.id));
		screenshot(hwnd, shot(dir, L"gui-3c-after-script").c_str());
	}

	if (resizeW > 0 && resizeH > 0) {
		ViewRect want(0, 0, resizeW, resizeH);
		view->checkSizeConstraint(&want);
		std::printf("host: resize to %dx%d -> allowed %dx%d\n", resizeW, resizeH, want.getWidth(), want.getHeight());
		setClientSize(hwnd, want.getWidth(), want.getHeight());
		view->onSize(&want);
		pump(0.8);
		printClientSize("resized", hwnd);
		screenshot(hwnd, shot(dir, L"gui-4-resized").c_str());
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
		screenshot(hwnd, shot(dir, L"gui-5-reopened").c_str());

	view->removed();
	view->release();
	DestroyWindow(hwnd);
	return 0;
}

// FERRITRON_TESTHOST_CLASSES=1: lists the factory's classes and exercises what a host does with
// a plug-in that has its own edit controller class, a compatibility class and renamed parameter
// IDs (VST 3.7.5 / 3.7.11): the controller class is created and asked for IEditController, the
// compatibility class for its JSON, and IRemapParamID for a parameter ID of an older version.
// FERRITRON_TESTHOST_OLD_PARAM=N names that old ID (default: the 1.3.0 hash of Bypass is not known
// here, so only the answer for an unknown ID is checked).
int checkClasses(IPluginFactory* factory) {
	// What a host shows for a class ID (FUID::toString): COM layout on Windows.
	auto hex = [](const TUID id) {
		uint8_t b[16];
		std::memcpy(b, id, 16);
#if COM_COMPATIBLE
		std::reverse(b, b + 4);
		std::reverse(b + 4, b + 6);
		std::reverse(b + 6, b + 8);
#endif
		char out[33];
		for (int i = 0; i < 16; ++i)
			std::snprintf(out + i * 2, 3, "%02X", b[i]);
		return std::string(out);
	};
	IPluginFactory2* factory2 = query<IPluginFactory2>(factory, IPluginFactory2_iid);
	const int32 count = factory->countClasses();
	std::printf("classes: %d\n", count);
	TUID audio{}, controllerClass{}, compatClass{};
	bool haveController = false, haveCompat = false;
	for (int32 i = 0; i < count; ++i) {
		PClassInfo2 two{};
		if (!factory2 || factory2->getClassInfo2(i, &two) != kResultOk)
			return fail("getClassInfo2 failed");
		std::printf("class %d: %s | %s | %s | %s\n", i, hex(two.cid).c_str(), two.category, two.name, two.subCategories);
		if (std::strcmp(two.category, "Audio Module Class") == 0 && i == 0)
			std::memcpy(audio, two.cid, sizeof(TUID));
		if (std::strcmp(two.category, "Component Controller Class") == 0) {
			std::memcpy(controllerClass, two.cid, sizeof(TUID));
			haveController = true;
		}
		if (std::strcmp(two.category, "Plugin Compatibility Class") == 0) {
			std::memcpy(compatClass, two.cid, sizeof(TUID));
			haveCompat = true;
		}
	}

	void* obj = nullptr;
	if (factory->createInstance(audio, IComponent_iid, &obj) != kResultOk)
		return fail("cannot create the component");
	auto* component = static_cast<IComponent*>(obj);
	component->initialize(nullptr);
	TUID named{};
	const bool separate = component->getControllerClassId(named) == kResultOk;
	std::printf("component: getControllerClassId -> %s%s\n", separate ? "separate controller " : "single object",
	            separate ? hex(named).c_str() : "");
	if (separate != haveController)
		return fail("getControllerClassId and the factory's controller class disagree");
	if (separate && std::memcmp(named, controllerClass, sizeof(TUID)) != 0)
		return fail("getControllerClassId names another class than the factory lists");

	IEditController* controller = nullptr;
	if (separate) {
		void* c = nullptr;
		if (factory->createInstance(controllerClass, IEditController_iid, &c) != kResultOk)
			return fail("cannot create the controller class");
		controller = static_cast<IEditController*>(c);
		controller->initialize(nullptr);
		// The host hands the component's state to the controller.
		MemoryStream state;
		component->getState(&state);
		state.at = 0;
		if (controller->setComponentState(&state) != kResultOk)
			return fail("the controller refused the component's state");
		std::printf("controller: created as its own object, %d parameters\n", controller->getParameterCount());
	} else {
		controller = query<IEditController>(component, IEditController_iid);
		if (!controller)
			return fail("the component has no IEditController");
	}

	if (auto* remap = query<IRemapParamID>(controller, IRemapParamID_iid)) {
		std::printf("IRemapParamID: available\n");
		// Parameter 0's current ID maps to itself; a made up ID does not map.
		ParameterInfo param{};
		controller->getParameterInfo(0, param);
		ParamID mapped = 0;
		const bool same = remap->getCompatibleParamID(audio, param.id, mapped) == kResultTrue && mapped == param.id;
		std::printf("IRemapParamID: parameter %u -> %s\n", param.id, same ? "same" : "different");
		ParamID unknown = 0;
		const bool none = remap->getCompatibleParamID(audio, 0x3ABCDEF1, unknown) != kResultTrue;
		std::printf("IRemapParamID: unknown ID -> %s\n", none ? "no mapping" : "mapped (wrong)");
		if (const char* old = std::getenv("FERRITRON_TESTHOST_OLD_PARAM")) {
			ParamID next = 0;
			const bool ok = remap->getCompatibleParamID(audio, static_cast<ParamID>(std::strtoul(old, nullptr, 10)), next) == kResultTrue;
			std::printf("IRemapParamID: old %s -> %s%u\n", old, ok ? "" : "none ", ok ? next : 0u);
		}
		TUID other{};
		other[0] = 1;
		const bool foreign = remap->getCompatibleParamID(other, param.id, mapped) != kResultTrue;
		std::printf("IRemapParamID: another plug-in -> %s\n", foreign ? "no mapping" : "mapped (wrong)");
		remap->release();
		if (!same || !none || !foreign)
			return fail("IRemapParamID answered wrongly");
	} else {
		std::printf("IRemapParamID: not offered\n");
	}

	if (haveCompat) {
		void* c = nullptr;
		if (factory->createInstance(compatClass, IPluginCompatibility_iid, &c) != kResultOk)
			return fail("cannot create the compatibility class");
		auto* compat = static_cast<IPluginCompatibility*>(c);
		MemoryStream json;
		if (compat->getCompatibilityJSON(&json) != kResultTrue)
			return fail("getCompatibilityJSON failed");
		std::printf("compatibility: %.*s\n", static_cast<int>(json.data.size()), reinterpret_cast<const char*>(json.data.data()));
		compat->release();
	}
	if (separate) {
		controller->terminate();
		controller->release();
	}
	component->terminate();
	component->release();
	std::printf("classes: ok\n");
	return 0;
}

#ifdef FERRITRON_HAVE_VST2
// VST 2: a plugin library that exports VSTPluginMain (and not GetPluginFactory) is loaded as a
// VST 2 effect: its editor opens in a window (effEditGetRect / effEditOpen), the host changes a
// parameter, clicks, resizes on request and closes it (effEditClose), with screenshots.
HWND g_vst2Window = nullptr;

VstIntPtr VSTCALLBACK vst2Callback(AEffect*, VstInt32 opcode, VstInt32 index, VstIntPtr value, void*, float opt) {
	switch (opcode) {
	case audioMasterVersion:
		return 2400;
	case audioMasterAutomate:
		std::printf("host: automate parameter %d = %.4f\n", index, opt);
		return 0;
	case audioMasterBeginEdit:
		std::printf("host: begin edit %d\n", index);
		return 1;
	case audioMasterEndEdit:
		std::printf("host: end edit %d\n", index);
		return 1;
	case audioMasterSizeWindow:
		std::printf("host: the editor asks for %dx%d\n", index, static_cast<int>(value));
		if (g_vst2Window)
			setClientSize(g_vst2Window, index, static_cast<int>(value));
		return 1;
	case audioMasterGetVendorString:
		return 0;
	default:
		return 0;
	}
}

int runVst2(HMODULE module, const wchar_t* shotDir, int clickX, int clickY, double scale, int resizeW, int resizeH) {
	using Vst2Main = AEffect* (*)(audioMasterCallback);
	auto main = reinterpret_cast<Vst2Main>(GetProcAddress(module, "VSTPluginMain"));
	AEffect* effect = main ? main(vst2Callback) : nullptr;
	if (!effect || effect->magic != kEffectMagic)
		return fail("VSTPluginMain gave no AEffect");
	auto call = [&](VstInt32 opcode, VstInt32 index = 0, VstIntPtr value = 0, void* ptr = nullptr, float opt = 0.0f) {
		return effect->dispatcher(effect, opcode, index, value, ptr, opt);
	};
	call(effOpen);
	char name[256] = {};
	call(effGetEffectName, 0, 0, name);
	std::printf("plugin: %s (VST 2, %d parameters, %s editor)\n", name, effect->numParams,
	            (effect->flags & effFlagsHasEditor) ? "has an" : "no");
	call(effSetSampleRate, 0, 0, nullptr, 48000.0f);
	call(effSetBlockSize, 0, 256);
	call(effMainsChanged, 0, 1);
	if (!(effect->flags & effFlagsHasEditor)) {
		call(effClose);
		return fail("the plugin has no editor");
	}

	ERect* rect = nullptr;
	if (!call(effEditGetRect, 0, 0, &rect) || !rect)
		return fail("effEditGetRect gave nothing");
	std::printf("editor: %dx%d\n", rect->right - rect->left, rect->bottom - rect->top);

	WNDCLASSW wc{};
	wc.lpfnWndProc = hostProc;
	wc.hInstance = GetModuleHandleW(nullptr);
	wc.lpszClassName = L"FerritronTestHost";
	RegisterClassW(&wc);
	RECT r{0, 0, rect->right - rect->left, rect->bottom - rect->top};
	AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
	HWND hwnd = CreateWindowW(wc.lpszClassName, L"Ferritron test host (VST 2)", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 100, 100,
	                          r.right - r.left, r.bottom - r.top, nullptr, nullptr, wc.hInstance, nullptr);
	g_vst2Window = hwnd;
	auto attachStart = std::chrono::steady_clock::now();
	if (!call(effEditOpen, 0, 0, hwnd))
		return fail("effEditOpen failed");
	std::printf("timing: editor opened in %.1f ms\n", msSince(attachStart));
	printClientSize("opened", hwnd);

	std::wstring dir = shotDir ? shotDir : L".";
	if (scale <= 0)
		scale = 1.0;
	for (int i = 0; i < 4; ++i) {
		pump(0.5);
		call(effEditIdle);
	}
	screenshot(hwnd, shot(dir, L"vst2-1-initial").c_str());

	effect->setParameter(effect, 0, 0.25f); // as automation would
	std::printf("host: set parameter 0 to 0.25\n");
	pump(0.5);
	screenshot(hwnd, shot(dir, L"vst2-2-after-host-change").c_str());

	click(hwnd, static_cast<int>(clickX * scale), static_cast<int>(clickY * scale));
	pump(0.8);
	std::printf("plugin: parameter 0 is now %.4f\n", effect->getParameter(effect, 0));
	screenshot(hwnd, shot(dir, L"vst2-3-after-click").c_str());

	if (resizeW > 0 && resizeH > 0) {
		// The host resizes its window: the editor follows (effEditGetRect again, the view sizes itself).
		setClientSize(hwnd, resizeW, resizeH);
		pump(0.8);
		printClientSize("resized", hwnd);
		screenshot(hwnd, shot(dir, L"vst2-4-resized").c_str());
	}

	call(effEditClose);
	DestroyWindow(hwnd);
	g_vst2Window = nullptr;
	call(effMainsChanged, 0, 0);
	call(effClose);
	std::printf("vst2: ok\n");
	return 0;
}
#endif

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
#ifdef FERRITRON_HAVE_VST2
	// The same library may hold VST 3 and VST 2 entry points: `<name>.vst2.dll` (or
	// FERRITRON_TESTHOST_VST2=1) loads it as a VST 2 plugin.
	if ((std::getenv("FERRITRON_TESTHOST_VST2") || std::wcsstr(binary, L".vst2.")) && GetProcAddress(module, "VSTPluginMain"))
		return runVst2(module, guiShotDir, clickX, clickY, scale, resizeW, resizeH);
#endif
	if (auto init = reinterpret_cast<InitDll>(GetProcAddress(module, "InitDll")))
		init();
	auto getFactory = reinterpret_cast<GetFactory>(GetProcAddress(module, "GetPluginFactory"));
	IPluginFactory* factory = getFactory ? getFactory() : nullptr;
	if (!factory || factory->countClasses() < 1)
		return fail("no plugin factory");

	PClassInfo info{};
	factory->getClassInfo(0, &info);
	std::printf("plugin: %s\n", info.name);
	if (std::getenv("FERRITRON_TESTHOST_CLASSES"))
		return checkClasses(factory);

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
		// FERRITRON_TESTHOST_INSTANCES=N (--instances): N instances in this
		// process, each on a track of its own; the editor opens for the first.
		std::vector<IComponent*> others;
		const char* count = envVar("FERRITRON_TESTHOST_INSTANCES");
		for (int i = 1; count && i < std::atoi(count); ++i) {
			void* more = nullptr;
			if (factory->createInstance(info.cid, IComponent_iid, &more) != kResultOk)
				return fail("createInstance failed");
			auto* c = static_cast<IComponent*>(more);
			c->initialize(nullptr);
			announceTrack(c, i);
			others.push_back(c);
		}
		if (count) {
			announceTrack(component, 0);
			std::printf("instances: %zu\n", others.size() + 1);
		}
		int code = runGui(controller, guiShotDir, clickX, clickY, scale, resizeW, resizeH);
		for (auto* c : others) {
			c->terminate();
			c->release();
		}
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
	// FERRITRON_TESTHOST_SIDECHAIN=LEVEL: the sidechain bus (if any) is switched on and fed LEVEL.
	const char* sidechainLevel = envVar("FERRITRON_TESTHOST_SIDECHAIN");
	if (sidechainLevel && component->getBusCount(kAudio, kInput) > 1)
		component->activateBus(kAudio, kInput, 1, true);
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
	const char* instances = envVar("FERRITRON_TESTHOST_INSTANCES");
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
	std::vector<float> scL(frames), scR(frames);
	float* scs[2] = {scL.data(), scR.data()};
	AudioBusBuffers inputs2[2]{};
	if (sidechainLevel && component->getBusCount(kAudio, kInput) > 1) {
		std::fill(scL.begin(), scL.end(), static_cast<float>(std::atof(sidechainLevel)));
		std::fill(scR.begin(), scR.end(), static_cast<float>(std::atof(sidechainLevel)));
		inputs2[0] = input;
		inputs2[1].numChannels = 2;
		inputs2[1].channelBuffers32 = scs;
		data.numInputs = 2;
		data.inputs = inputs2;
	}
	// FERRITRON_TESTHOST_TEMPO=BPM: the host plays at that tempo (ProcessContext).
	ProcessContext context{};
	if (const char* tempo = envVar("FERRITRON_TESTHOST_TEMPO")) {
		context.state = ProcessContext::kPlaying | ProcessContext::kTempoValid;
		context.tempo = std::atof(tempo);
		data.processContext = &context;
	}
	data.outputs = &output;
	DenseChanges dense;
	OutChanges outChanges;
	OutEvents outEvents;
	if (envVar("FERRITRON_TESTHOST_OUT")) {
		data.outputParameterChanges = &outChanges;
		data.outputEvents = &outEvents;
	}
	if (const char* n = envVar("FERRITRON_TESTHOST_DENSE")) {
		ParameterInfo first{};
		controller->getParameterInfo(target, first);
		dense.queue.id = first.id;
		dense.queue.count = std::atoi(n);
		dense.queue.frames = frames;
		data.inputParameterChanges = &dense;
		std::printf("host: %d automation points per block on parameter %u\n", dense.queue.count, first.id);
	}

	// FERRITRON_TESTHOST_BENCH=N (`--bench N`): time N blocks back to back (S7, S9).
	if (const char* bench = envVar("FERRITRON_TESTHOST_BENCH")) {
		const int n = (std::max)(1, std::atoi(bench));
		auto block = [&] {
			std::fill(inL.begin(), inL.end(), 0.25f);
			std::fill(inR.begin(), inR.end(), 0.25f);
			processor->process(data);
		};
		for (int i = 0; i < 500; ++i)
			block();
		std::vector<double> us(static_cast<size_t>(n));
		for (int i = 0; i < n; ++i) {
			auto t0 = std::chrono::steady_clock::now();
			block();
			us[static_cast<size_t>(i)] = std::chrono::duration<double, std::micro>(std::chrono::steady_clock::now() - t0).count();
		}
		double sum = 0;
		for (double u : us)
			sum += u;
		std::sort(us.begin(), us.end());
		std::printf("bench: %d blocks of %d frames, stereo, 48 kHz: mean %.2f us, p99 %.2f us, max %.2f us (x%.0f realtime)\n", n, frames, sum / n,
		            us[static_cast<size_t>(n * 99 / 100)], us.back(), (frames / 48000.0 * 1e6) / (sum / n));
		seconds = -1; // skip the listening loop
	}

	float last = NAN;
	auto start = std::chrono::steady_clock::now();
	ParamID pulseId = 0;
	if (pulseAt >= 0) {
		ParameterInfo first{};
		controller->getParameterInfo(target, first);
		pulseId = first.id;
	}
	bool pulsed = false;
	for (; seconds >= 0;) {
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
		if (outL[frames - 1] != last) {
			last = outL[frames - 1];
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
