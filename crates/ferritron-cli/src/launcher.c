/*
 * The executable of a Ferritron standalone app (and of an AUv3 app
 * extension): loads the plugin binary, whose path relative to this
 * executable is FERRITRON_LIBRARY (launcher_config.h, written by
 * `cargo ferritron bundle`), and calls its entry point.
 *
 * FERRITRON_APPEX: an app extension. The binary is loaded before main (the
 * extension's principal class lives in it) and the program starts at
 * NSExtensionMain (linked with -e _NSExtensionMain).
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */
#include "launcher_config.h"

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>

int WINAPI wWinMain(HINSTANCE instance, HINSTANCE previous, PWSTR command_line, int show) {
    (void)instance; (void)previous; (void)command_line; (void)show;
    static wchar_t path[32768];
    DWORD n = GetModuleFileNameW(NULL, path, 32768);
    if (n == 0 || n >= 32768) return 1;
    wchar_t* slash = wcsrchr(path, L'\\');
    if (!slash) return 1;
    slash[1] = 0;
    size_t used = (size_t)(slash + 1 - path);
    if (!MultiByteToWideChar(CP_UTF8, 0, FERRITRON_LIBRARY, -1, slash + 1, (int)(32768 - used))) return 1;
    for (wchar_t* p = slash + 1; *p; p++) if (*p == L'/') *p = L'\\';
    HMODULE module = LoadLibraryW(path);
    if (!module) {
        MessageBoxW(NULL, path, L"Ferritron: cannot load the plugin", MB_ICONERROR);
        return 1;
    }
    int (*entry)(void) = (int (*)(void))(void*)GetProcAddress(module, "ferritron_standalone_main");
    return entry ? entry() : 1;
}

#else
#include <dlfcn.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifdef __APPLE__
#include <mach-o/dyld.h>
#endif
#ifdef __linux__
#include <unistd.h>
#endif

static void* load(void) {
    char exe[PATH_MAX], path[PATH_MAX * 2];
#ifdef __APPLE__
    uint32_t size = sizeof exe;
    if (_NSGetExecutablePath(exe, &size) != 0) return NULL;
#else
    ssize_t len = readlink("/proc/self/exe", exe, sizeof exe - 1);
    if (len <= 0) return NULL;
    exe[len] = 0;
#endif
    char* slash = strrchr(exe, '/');
    if (!slash) return NULL;
    slash[1] = 0;
    snprintf(path, sizeof path, "%s%s", exe, FERRITRON_LIBRARY);
    void* handle = dlopen(path, RTLD_NOW | RTLD_GLOBAL);
    if (!handle) fprintf(stderr, "ferritron: cannot load %s: %s\n", path, dlerror());
    return handle;
}

#ifdef FERRITRON_APPEX
__attribute__((constructor)) static void ferritron_load(void) { load(); }
#else
int main(void) {
    void* handle = load();
    if (!handle) return 1;
    int (*entry)(void) = (int (*)(void))dlsym(handle, "ferritron_standalone_main");
    return entry ? entry() : 1;
}
#endif
#endif
