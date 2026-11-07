// --shared-runtime: loads two Ferritron plugin libraries (format-independent
// entry point) into this process and checks that they share one Runtime,
// i.e. one set of "ferritron-worker" threads.

#include "ferritron_abi.h"

#include <tlhelp32.h>

namespace {

int countWorkers() {
	int count = 0;
	HANDLE snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
	THREADENTRY32 te{sizeof(te)};
	for (BOOL ok = Thread32First(snap, &te); ok; ok = Thread32Next(snap, &te)) {
		if (te.th32OwnerProcessID != GetCurrentProcessId())
			continue;
		HANDLE thread = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, FALSE, te.th32ThreadID);
		PWSTR name = nullptr;
		if (thread && SUCCEEDED(GetThreadDescription(thread, &name)) && name) {
			if (std::wcsncmp(name, L"ferritron-worker", 16) == 0)
				++count;
			LocalFree(name);
		}
		if (thread)
			CloseHandle(thread);
	}
	CloseHandle(snap);
	return count;
}

struct Loaded {
	HMODULE module = nullptr;
	const ferritron_plugin_api* api = nullptr;
	void* core = nullptr;
};

bool load(const wchar_t* path, Loaded& out) {
	out.module = LoadLibraryW(path);
	if (!out.module)
		return false;
	using Entry = const ferritron_plugin_api* (*)();
	auto entry = reinterpret_cast<Entry>(GetProcAddress(out.module, "ferritron_plugin_entry"));
	out.api = entry ? entry() : nullptr;
	if (!out.api)
		return false;
	out.core = out.api->create();
	return out.core && out.api->activate(out.core, 48000.0, 256);
}

bool processBlock(const Loaded& p) {
	std::vector<float> buf(256, 0.0f);
	float* ch[1] = {buf.data()};
	return p.api->process(p.core, ch, 1, 256, nullptr, 0, nullptr);
}

} // namespace

extern "C" int ferritron_testhost_shared_runtime(const wchar_t* pathA, const wchar_t* pathB) {
	Loaded a, b;
	if (!load(pathA, a))
		return fail("cannot load the first plugin library");
	std::printf("library A at %p: %d worker thread(s)\n", static_cast<void*>(a.module), countWorkers());
	if (!load(pathB, b))
		return fail("cannot load the second plugin library");
	std::printf("library B at %p: %d worker thread(s) after both instances exist\n", static_cast<void*>(b.module),
	            countWorkers());
	std::printf("process A: %s, process B: %s\n", processBlock(a) ? "ok" : "FAILED", processBlock(b) ? "ok" : "FAILED");

	// The library that owns the Runtime goes away first: B must keep working.
	a.api->destroy(a.core);
	FreeLibrary(a.module);
	std::printf("after unloading A: process B: %s, %d worker thread(s)\n", processBlock(b) ? "ok" : "FAILED",
	            countWorkers());
	b.api->destroy(b.core);
	std::this_thread::sleep_for(std::chrono::milliseconds(200));
	std::printf("after destroying B: %d worker thread(s)\n", countWorkers());
	FreeLibrary(b.module);
	return 0;
}
