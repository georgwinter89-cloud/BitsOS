// BR-2: loads fontconfig the way Brave does (libfontconfig.so.1 from the library set, FONTCONFIG_FILE set by the
// launcher) and prints how many fonts its configuration has. Run by ktest ul.pkg.brave_fonts with an empty HOME:
// if fontconfig found the tree's prebuilt cache invalid it rescans and writes a cache into $XDG_CACHE_HOME, which
// the test then sees. dlopen instead of -lfontconfig: xtask builds initrd programs without extra libraries.
#include <dlfcn.h>
#include <stdio.h>

typedef int FcBool;
typedef struct FcFontSet { int nfont; int sfont; void **fonts; } FcFontSet;

int main(void) {
    void *fc = dlopen("libfontconfig.so.1", RTLD_NOW);
    if (!fc) { fprintf(stderr, "fc-probe: %s\n", dlerror()); return 2; }
    FcBool (*init)(void) = (FcBool (*)(void))dlsym(fc, "FcInit");
    void *(*current)(void) = (void *(*)(void))dlsym(fc, "FcConfigGetCurrent");
    FcFontSet *(*fonts)(void *, int) = (FcFontSet *(*)(void *, int))dlsym(fc, "FcConfigGetFonts");
    if (!init || !current || !fonts) { fprintf(stderr, "fc-probe: fontconfig lacks FcInit/FcConfigGetFonts\n"); return 2; }
    if (!init()) { fprintf(stderr, "fc-probe: FcInit failed\n"); return 1; }
    FcFontSet *set = fonts(current(), 0 /* FcSetSystem */);
    printf("fc-probe: fonts %d\n", set ? set->nfont : -1);
    return set && set->nfont > 0 ? 0 : 1;
}
