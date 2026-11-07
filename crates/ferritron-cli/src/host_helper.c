/* ferritron-host: runs an isolated Ferritron plugin (crash isolation level 3)
   on macOS and Linux, where no system tool loads a library and calls into it
   (Windows uses rundll32.exe). `cargo ferritron bundle` compiles it into each
   bundle's Contents/Helpers.

     ferritron-host <plugin library> <arguments...>

   loads the library and calls its `ferritron_host_main` export with the
   arguments joined by spaces.

   SPDX-License-Identifier: MIT OR Apache-2.0 */

#include <dlfcn.h>
#include <stdio.h>
#include <string.h>

int main(int argc, char** argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: ferritron-host <plugin library> <arguments...>\n");
        return 2;
    }
    void* library = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    if (!library) {
        fprintf(stderr, "ferritron-host: %s\n", dlerror());
        return 1;
    }
    void (*entry)(const char*) = (void (*)(const char*))dlsym(library, "ferritron_host_main");
    if (!entry) {
        fprintf(stderr, "ferritron-host: %s has no ferritron_host_main\n", argv[1]);
        return 1;
    }
    char line[1024] = "";
    for (int i = 2; i < argc; ++i) {
        if (strlen(line) + strlen(argv[i]) + 2 > sizeof(line))
            return 2;
        if (i > 2)
            strcat(line, " ");
        strcat(line, argv[i]);
    }
    entry(line);
    return 0;
}
