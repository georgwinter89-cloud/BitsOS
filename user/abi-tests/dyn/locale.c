// UL-6: glibc's locales from /usr/lib/locale (C.utf8, de_DE.utf8, en_US.utf8 as directories, no locale-archive):
// which names setlocale accepts, nl_langinfo, localeconv, strftime, printf's grouping, wide characters, collation
// and newlocale/uselocale. Also /bin/pkg-locale (manifest), whose output is user/packaging/tests/pkg-locale.out.
// With the argument "env" it reports the locale the environment (LANG, LC_*) selects instead; with "lang" it
// first sets LANG=de_DE.UTF-8 (as init gives the console) and clears LC_ALL/LC_CTYPE, so setlocale(LC_ALL, "")
// must pick de_DE from LANG (its info lines then equal the de_DE.UTF-8 lines of the plain run).
#define _GNU_SOURCE
#include <langinfo.h>
#include <locale.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <wchar.h>
#include <wctype.h>

static void info(const char *tag) {
    struct lconv *lc = localeconv();
    printf("%s: codeset=%s radix='%s' thousep='%s' d_fmt='%s' t_fmt='%s' day1=%s mon3=%s yes=%s\n", tag,
           nl_langinfo(CODESET), nl_langinfo(RADIXCHAR), nl_langinfo(THOUSEP), nl_langinfo(D_FMT),
           nl_langinfo(T_FMT), nl_langinfo(DAY_1), nl_langinfo(ABMON_3), nl_langinfo(YESEXPR));
    printf("%s: lconv dp='%s' ts='%s' cur='%s' intcur='%s' mon_dp='%s' frac=%d p_cs_precedes=%d\n", tag,
           lc->decimal_point, lc->thousands_sep, lc->currency_symbol, lc->int_curr_symbol, lc->mon_decimal_point,
           lc->frac_digits, lc->p_cs_precedes);
    // Monday 2 March 2026, 14:05:09 UTC: a fixed date, nothing from the clock or the time zone.
    struct tm t = {.tm_year = 126, .tm_mon = 2, .tm_mday = 2, .tm_hour = 14, .tm_min = 5, .tm_sec = 9, .tm_wday = 1, .tm_yday = 60, .tm_zone = "UTC"};
    char buf[128];
    strftime(buf, sizeof buf, "%A %a %B %b %x %X %c", &t);
    printf("%s: strftime '%s'\n", tag, buf);
    printf("%s: printf '%'.2f' '%'d'\n", tag, 1234567.891, 9876543);
    wchar_t w[16];
    size_t n = mbstowcs(w, "Gr\xc3\xbc\xc3\x9f" "e", 16);
    printf("%s: mbstowcs=%zd towupper(u-umlaut)=%#x iswalpha(sharp-s)=%d\n", tag, (ssize_t)n, (unsigned)towupper(0xfc),
           iswalpha(0xdf) != 0);
    int c = strcoll("\xc3\xa4pfel", "birne");
    printf("%s: strcoll(apfel-umlaut, birne) %s 0\n", tag, c < 0 ? "<" : c > 0 ? ">" : "==");
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc > 1 && strcmp(argv[1], "lang") == 0) {
        unsetenv("LC_ALL");
        unsetenv("LC_CTYPE");
        unsetenv("LC_MESSAGES");
        setenv("LANG", "de_DE.UTF-8", 1);
        const char *r = setlocale(LC_ALL, "");
        printf("lang: setlocale(LC_ALL, \"\") = %s\n", r ? r : "(null)");
        if (!r)
            return 1;
        info(r);
        return 0;
    }
    if (argc > 1 && strcmp(argv[1], "env") == 0) {
        const char *r = setlocale(LC_ALL, "");
        printf("env: setlocale(LC_ALL, \"\") = %s\n", r ? r : "(null)");
        info("env");
        return r ? 0 : 1;
    }
    printf("start: %s\n", setlocale(LC_ALL, NULL));
    const char *names[] = {"C", "POSIX", "C.UTF-8", "C.utf8", "en_US.UTF-8", "en_US.utf8", "de_DE.UTF-8",
                           "de_DE.utf8", "de_DE", "en_US", "de_DE.ISO-8859-1", "xx_YY.UTF-8", ""};
    for (size_t i = 0; names[i][0]; i++) {
        const char *r = setlocale(LC_ALL, names[i]);
        printf("setlocale(LC_ALL, \"%s\") = %s\n", names[i], r ? r : "(null)");
    }
    const char *use[] = {"C", "C.UTF-8", "en_US.UTF-8", "de_DE.UTF-8"};
    for (size_t i = 0; i < sizeof use / sizeof use[0]; i++) {
        if (!setlocale(LC_ALL, use[i])) {
            printf("%s: missing\n", use[i]);
            continue;
        }
        info(use[i]);
    }
    setlocale(LC_ALL, "C");
    // A mixed locale: German time and numbers, English messages.
    setlocale(LC_TIME, "de_DE.UTF-8");
    setlocale(LC_NUMERIC, "de_DE.UTF-8");
    printf("mixed: %s\n", setlocale(LC_ALL, NULL));
    setlocale(LC_ALL, "C");
    locale_t de = newlocale(LC_ALL_MASK, "de_DE.UTF-8", (locale_t)0);
    locale_t bad = newlocale(LC_ALL_MASK, "xx_YY.UTF-8", (locale_t)0);
    printf("newlocale de=%s bad=%s\n", de ? "ok" : "fail", bad ? "ok" : "fail");
    if (de) {
        printf("nl_langinfo_l: day2=%s codeset=%s\n", nl_langinfo_l(DAY_2, de), nl_langinfo_l(CODESET, de));
        locale_t old = uselocale(de);
        printf("uselocale: radix='%s' global still '%s'\n", nl_langinfo(RADIXCHAR), setlocale(LC_ALL, NULL));
        uselocale(old);
        printf("uselocale back: radix='%s'\n", nl_langinfo(RADIXCHAR));
        freelocale(de);
    }
    printf("done\n");
    return 0;
}
