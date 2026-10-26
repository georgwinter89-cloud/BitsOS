/* SN-4: the ALSA kernel interface with raw ioctls, as libasound's hw plugin uses it (V2 only: never run on a
 * host with a sound card; no libasound, so the transcript shows the kernel's answers). Each line is
 * "<what> <value>"; host/tests/sn/alsa-hw.expected holds the answers linux-6.6/sound/core gives for this
 * device's constraints (plan/notes/sn-SN-4.md). "alsa-hw selftest" plays 1 s of the "hw" signal (mixcheck.py
 * alsa). SN-12: without a word it prints the usage and exits 2 (it used to play: an agent asking for the state
 * made the Mini-PC beep); "alsa-hw state" prints the mixer controls without writing or playing anything. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>
#include <sound/asound.h>

static int xi(int fd, unsigned long cmd, void *arg) {
    return ioctl(fd, cmd, arg) < 0 ? -errno : 0;
}

static struct snd_interval *iv(struct snd_pcm_hw_params *p, int k) {
    return &p->intervals[k - SNDRV_PCM_HW_PARAM_FIRST_INTERVAL];
}

static void any(struct snd_pcm_hw_params *p) {
    memset(p, 0, sizeof(*p));
    for (int k = 0; k < 3; k++)
        memset(&p->masks[k], 0xff, sizeof(p->masks[k]));
    for (int k = SNDRV_PCM_HW_PARAM_FIRST_INTERVAL; k <= SNDRV_PCM_HW_PARAM_LAST_INTERVAL; k++)
        iv(p, k)->max = ~0u;
    p->rmask = ~0u;
    p->info = ~0u;
}

static void fix(struct snd_pcm_hw_params *p, int k, unsigned v) {
    struct snd_interval *i = iv(p, k);
    i->min = i->max = v;
    i->integer = 1;
}

static void mask1(struct snd_pcm_hw_params *p, int k, unsigned v) {
    memset(&p->masks[k], 0, sizeof(p->masks[k]));
    p->masks[k].bits[v / 32] = 1u << (v % 32);
}

static int state(int fd) {
    struct snd_pcm_status s;
    memset(&s, 0, sizeof(s));
    return xi(fd, SNDRV_PCM_IOCTL_STATUS, &s) ? -1 : (int)s.state;
}

/* The "hw" signal of mixcheck.py: a sawtooth on the left, its negative half-speed on the right. */
static short sig(unsigned n, int ch) {
    return ch == 0 ? (short)(((n * 37) & 0x3fff) - 8192) : (short)(8191 - ((n * 19) & 0x3fff));
}

/* "alsa-hw jack": the headphone jack element; "alsa-hw master N": master volume N (0..32768) on both
 * channels; "alsa-hw switch 0|1": the master switch on both channels (SN-8b: 0 also mutes a USB headset's
 * feature units); SN-9 "alsa-hw cswitch 0|1" / "alsa-hw cvolume N": Capture Switch / Capture Volume (the
 * Mini-PC check list; no amixer on V2). */
static int usage(void) {
    fprintf(stderr, "usage: alsa-hw state | jack | master [N] | switch [0|1] | cswitch [0|1] | cvolume [N]\n"
                    "       alsa-hw cobalt [SECONDS [KEEP]] | capture [SECONDS [FILE]] | selftest (plays 1 s)\n");
    return 2;
}

/* A number argument: all digits (an optional minus), else the usage (a typo never becomes a write). */
static int number(const char *a, long *out) {
    char *end;
    if (!*a)
        return 0;
    errno = 0;
    *out = strtol(a, &end, 10);
    return !*end && !errno;
}

/* "alsa-hw state": every mixer control the card has, read only ("<word> <ret> <left> <right>"; ret -2 = the
 * card has no such control). No write, no PCM open: silent. */
static int show_state(void) {
    static const struct { const char *word, *name; int iface; } els[] = {
        {"jack", "Headphone Jack", SNDRV_CTL_ELEM_IFACE_CARD},
        {"master", "Master Playback Volume", SNDRV_CTL_ELEM_IFACE_MIXER},
        {"switch", "Master Playback Switch", SNDRV_CTL_ELEM_IFACE_MIXER},
        {"cvolume", "Capture Volume", SNDRV_CTL_ELEM_IFACE_MIXER},
        {"cswitch", "Capture Switch", SNDRV_CTL_ELEM_IFACE_MIXER},
    };
    int c = open("/dev/snd/controlC0", O_RDONLY);
    if (c < 0)
        return printf("open controlC0 %d\n", -errno), 1;
    for (unsigned i = 0; i < sizeof(els) / sizeof(els[0]); i++) {
        struct snd_ctl_elem_value v;
        memset(&v, 0, sizeof(v));
        v.id.iface = els[i].iface;
        strcpy((char *)v.id.name, els[i].name);
        int r = xi(c, SNDRV_CTL_IOCTL_ELEM_READ, &v);
        printf("%s %d %ld %ld\n", els[i].word, r, v.value.integer.value[0], v.value.integer.value[1]);
    }
    close(c);
    return 0;
}

static int elem(int argc, char **argv) {
    const char *w = argv[1];
    long val = 0;
    if (strcmp(w, "jack") && strcmp(w, "master") && strcmp(w, "switch") && strcmp(w, "cswitch") && strcmp(w, "cvolume"))
        return usage();
    if (argc > 3 || (argc > 2 && (!strcmp(w, "jack") || !number(argv[2], &val))))
        return usage();
    int c = open("/dev/snd/controlC0", O_RDWR);
    struct snd_ctl_elem_value v;
    memset(&v, 0, sizeof(v));
    if (c < 0)
        return printf("open controlC0 %d\n", -errno), 1;
    if (strcmp(argv[1], "jack") == 0) {
        v.id.iface = SNDRV_CTL_ELEM_IFACE_CARD;
        strcpy((char *)v.id.name, "Headphone Jack");
        int r = xi(c, SNDRV_CTL_IOCTL_ELEM_READ, &v);
        printf("jack %d %ld\n", r, v.value.integer.value[0]);
        return r != 0;
    }
    int cap = argv[1][0] == 'c';
    int sw = strcmp(argv[1] + cap, "switch") == 0;
    v.id.iface = SNDRV_CTL_ELEM_IFACE_MIXER;
    strcpy((char *)v.id.name, cap ? (sw ? "Capture Switch" : "Capture Volume") : sw ? "Master Playback Switch" : "Master Playback Volume");
    v.value.integer.value[0] = v.value.integer.value[1] = argc > 2 ? val : (sw ? 1 : 32768);
    int r = xi(c, SNDRV_CTL_IOCTL_ELEM_WRITE, &v);
    printf("%s %d %ld\n", argv[1], r, v.value.integer.value[0]);
    return r != 0;
}

/* SN-7 "alsa-hw cobalt SECONDS [KEEP]": Cobalt's ALSA sink (starboard/shared/alsa) with raw ioctls: FLOAT_LE
 * 48 kHz stereo, period 256, buffer 8192, start 1024, stop 8192, avail_min 512, silence threshold 256; it writes 512 frames
 * whenever DELAY < KEEP (2048) and sleeps 5333 us otherwise; an XRUN (EPIPE) is counted and prepared away as
 * AlsaGetBufferedFrames does. It plays the "hw" sawtooth without a break, so the WAV shows any gap
 * (mixcheck.py cobalt). Prints "cobalt xruns X frames N delay MIN..MAX stalls S maxgap G ms", then one line
 * "xrun T ms gap G ms" per XRUN (T = CLOCK_MONOTONIC, as the kernel log; G = the longest the writer itself was
 * away within the 100 ms before it): a writer kept away for longer than its queue lasts (a QEMU guest stalled
 * under host load) starves on any system; 40-cobalt.sh tells those apart from logic XRUNs. */
static long mono_us(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000000L + t.tv_nsec / 1000;
}

static int cobalt(int argc, char **argv) {
    long secs = argc > 2 ? atol(argv[2]) : 120, keep = argc > 3 ? atol(argv[3]) : 2048;
    int fd = open("/dev/snd/pcmC0D0p", O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        printf("open pcmC0D0p %d\n", -errno);
        return 1;
    }
    struct snd_pcm_hw_params p;
    any(&p);
    mask1(&p, SNDRV_PCM_HW_PARAM_ACCESS, SNDRV_PCM_ACCESS_RW_INTERLEAVED);
    mask1(&p, SNDRV_PCM_HW_PARAM_FORMAT, SNDRV_PCM_FORMAT_FLOAT_LE);
    fix(&p, SNDRV_PCM_HW_PARAM_CHANNELS, 2);
    fix(&p, SNDRV_PCM_HW_PARAM_RATE, 48000);
    fix(&p, SNDRV_PCM_HW_PARAM_PERIOD_SIZE, 256); /* what libasound chose for Cobalt (plan/notes/yt-YT-4a.md) */
    fix(&p, SNDRV_PCM_HW_PARAM_BUFFER_SIZE, 8192);
    int r = xi(fd, SNDRV_PCM_IOCTL_HW_PARAMS, &p);
    if (r) {
        printf("hw_params %d\n", r);
        return 1;
    }
    struct snd_pcm_sw_params sw;
    memset(&sw, 0, sizeof(sw));
    sw.proto = SNDRV_PCM_VERSION;
    sw.avail_min = 512;
    sw.start_threshold = 1024;
    sw.stop_threshold = 8192;
    sw.silence_threshold = 256;
    if ((r = xi(fd, SNDRV_PCM_IOCTL_SW_PARAMS, &sw)) || (r = xi(fd, SNDRV_PCM_IOCTL_PREPARE, NULL))) {
        printf("sw_params/prepare %d\n", r);
        return 1;
    }
    float buf[512 * 2];
    unsigned long n = 0, total = (unsigned long)secs * 48000;
    long xruns = 0, dmin = 1L << 40, dmax = 0, stalls = 0, maxgap = 0, last = mono_us(), big_at = 0, big = 0;
    static long xat[256], xgap[256];
    while (n < total) {
        long now = mono_us(), gap = now - last, d = 0;
        last = now;
        /* Away longer than one 5333 us sleep plus 10 ms. */
        if (gap > 15333) {
            stalls++;
            big = now - big_at > 100000 || gap > big ? gap : big;
            big_at = now;
        }
        if (gap > maxgap)
            maxgap = gap;
        r = xi(fd, SNDRV_PCM_IOCTL_DELAY, &d);
        if (r == -EPIPE) {
            if (xruns < 256) {
                xat[xruns] = now / 1000;
                xgap[xruns] = now - big_at <= 100000 ? big / 1000 : 0;
            }
            xruns++;
            xi(fd, SNDRV_PCM_IOCTL_PREPARE, NULL);
            continue;
        }
        if (r) {
            printf("delay %d\n", r);
            return 1;
        }
        if (n > 48000 && d < dmin)
            dmin = d;
        if (d > dmax)
            dmax = d;
        if (d >= keep) {
            usleep(5333);
            continue;
        }
        for (unsigned i = 0; i < 512; i++)
            for (int ch = 0; ch < 2; ch++)
                buf[i * 2 + ch] = sig(n + i, ch) / 32768.0f;
        struct snd_xferi x = {.buf = buf, .frames = 512};
        r = xi(fd, SNDRV_PCM_IOCTL_WRITEI_FRAMES, &x);
        if (r == -EPIPE) {
            if (xruns < 256) {
                xat[xruns] = mono_us() / 1000;
                xgap[xruns] = mono_us() - big_at <= 100000 ? big / 1000 : 0;
            }
            xruns++;
            xi(fd, SNDRV_PCM_IOCTL_PREPARE, NULL);
            continue;
        }
        if (r || x.result != 512) {
            printf("writei %d result %ld\n", r, (long)x.result);
            return 1;
        }
        n += 512;
    }
    r = xi(fd, SNDRV_PCM_IOCTL_DRAIN, NULL);
    printf("cobalt xruns %ld frames %lu delay %ld..%ld stalls %ld maxgap %ld ms drain %d\n", xruns, n, dmin, dmax, stalls,
           maxgap / 1000, r);
    for (long i = 0; i < xruns && i < 256; i++)
        printf("xrun %ld ms gap %ld ms\n", xat[i], xgap[i]);
    close(fd);
    return xruns || r ? 1 : 0;
}

/* SN-9 "alsa-hw capture SECONDS [FILE]": the capture substream with raw ioctls, as arecord -D hw:0,0 drives
 * it: open (O_NONBLOCK, then blocking), a second open (EBUSY: one substream), refine, hw_params S16 stereo
 * 48 kHz, sw_params, prepare, READI of SECONDS s in periods (the first starts the stream), status, delay,
 * an XRUN after sleeping past the buffer, drop, hw_free. Lines starting with "# " vary (levels, timing) and
 * are not compared; host/tests/sn/capture.expected holds the rest as linux-6.6 answers for this device.
 * FILE gets the frames as raw S16_LE stereo 48 kHz (aplay -t raw -f S16_LE -r 48000 -c 2 FILE). */
static int capture(int argc, char **argv) {
    long secs = argc > 2 ? atol(argv[2]) : 2;
    int fd = open("/dev/snd/pcmC0D0c", O_RDWR | O_NONBLOCK | O_CLOEXEC);
    if (fd < 0) {
        printf("open pcmC0D0c %d\n", -errno);
        return 1;
    }
    fcntl(fd, F_SETFL, O_RDWR);
    int second = open("/dev/snd/pcmC0D0c", O_RDWR | O_NONBLOCK);
    printf("cap.open2 %d\n", second < 0 ? -errno : 0);
    if (second >= 0)
        close(second);
    int v = 0;
    int r = xi(fd, SNDRV_PCM_IOCTL_PVERSION, &v);
    printf("cap.pversion %d %#x\n", r, v);
    struct snd_pcm_info info;
    r = xi(fd, SNDRV_PCM_IOCTL_INFO, &info);
    printf("cap.info %d stream %d %s %u/%u\n", r, info.stream, (char *)info.name, info.subdevices_avail, info.subdevices_count);
    struct snd_xferi x = {0};
    short *buf = calloc(1200 * 2, sizeof(short));
    x.buf = buf;
    x.frames = 16;
    printf("readi.open %d\n", xi(fd, SNDRV_PCM_IOCTL_READI_FRAMES, &x));
    struct snd_pcm_hw_params p;
    any(&p);
    r = xi(fd, SNDRV_PCM_IOCTL_HW_REFINE, &p);
    printf("refine %d access %#x format %#x channels %u-%u rate %u-%u sample_bits %u-%u buffer_bytes_max %u\n", r,
           p.masks[0].bits[0], p.masks[1].bits[0], iv(&p, SNDRV_PCM_HW_PARAM_CHANNELS)->min, iv(&p, SNDRV_PCM_HW_PARAM_CHANNELS)->max,
           iv(&p, SNDRV_PCM_HW_PARAM_RATE)->min, iv(&p, SNDRV_PCM_HW_PARAM_RATE)->max, iv(&p, SNDRV_PCM_HW_PARAM_SAMPLE_BITS)->min,
           iv(&p, SNDRV_PCM_HW_PARAM_SAMPLE_BITS)->max, iv(&p, SNDRV_PCM_HW_PARAM_BUFFER_BYTES)->max);
    any(&p);
    fix(&p, SNDRV_PCM_HW_PARAM_RATE, 44100);
    printf("refine.44100 %d\n", xi(fd, SNDRV_PCM_IOCTL_HW_REFINE, &p));
    any(&p);
    mask1(&p, SNDRV_PCM_HW_PARAM_ACCESS, SNDRV_PCM_ACCESS_RW_INTERLEAVED);
    mask1(&p, SNDRV_PCM_HW_PARAM_FORMAT, SNDRV_PCM_FORMAT_S16_LE);
    fix(&p, SNDRV_PCM_HW_PARAM_CHANNELS, 2);
    fix(&p, SNDRV_PCM_HW_PARAM_RATE, 48000);
    fix(&p, SNDRV_PCM_HW_PARAM_PERIOD_SIZE, 1200);
    fix(&p, SNDRV_PCM_HW_PARAM_BUFFER_SIZE, 4800);
    r = xi(fd, SNDRV_PCM_IOCTL_HW_PARAMS, &p);
    printf("hw_params %d period %u periods %u buffer %u bytes %u msbits %u rate %u/%u pause %d mmap %d\n", r,
           iv(&p, SNDRV_PCM_HW_PARAM_PERIOD_SIZE)->min, iv(&p, SNDRV_PCM_HW_PARAM_PERIODS)->min, iv(&p, SNDRV_PCM_HW_PARAM_BUFFER_SIZE)->min,
           iv(&p, SNDRV_PCM_HW_PARAM_BUFFER_BYTES)->min, p.msbits, p.rate_num, p.rate_den, !!(p.info & SNDRV_PCM_INFO_PAUSE),
           !!(p.info & SNDRV_PCM_INFO_MMAP));
    if (r) {
        printf("# no capture setup (no microphone or line input?)\n");
        return 1;
    }
    printf("state.setup %d\n", state(fd));
    printf("readi.setup %d\n", xi(fd, SNDRV_PCM_IOCTL_READI_FRAMES, &x));
    struct snd_pcm_sw_params sw;
    memset(&sw, 0, sizeof(sw));
    sw.proto = SNDRV_PCM_VERSION;
    sw.avail_min = 1200;
    sw.start_threshold = 1;
    sw.stop_threshold = 4800;
    r = xi(fd, SNDRV_PCM_IOCTL_SW_PARAMS, &sw);
    printf("sw_params %d boundary %s\n", r, sw.boundary >= 4800ul << 40 && sw.boundary % 4800 == 0 ? "ok" : "bad");
    printf("prepare %d\n", xi(fd, SNDRV_PCM_IOCTL_PREPARE, NULL));
    printf("state.prepared %d\n", state(fd));
    x.buf = (void *)16;
    x.frames = 100;
    printf("readi.efault %d\n", xi(fd, SNDRV_PCM_IOCTL_READI_FRAMES, &x));
    FILE *out = argc > 3 ? fopen(argv[3], "wb") : NULL;
    unsigned long want = (unsigned long)secs * 48000, got = 0;
    int peak = 0, err = 0;
    x.buf = buf;
    while (got < want && !err) {
        x.frames = 1200;
        err = xi(fd, SNDRV_PCM_IOCTL_READI_FRAMES, &x);
        if (err == -EPIPE) {
            printf("# xrun after %lu frames\n", got);
            err = xi(fd, SNDRV_PCM_IOCTL_PREPARE, NULL);
            continue;
        }
        if (err)
            break;
        for (long i = 0; i < x.result * 2; i++)
            peak = abs(buf[i]) > peak ? abs(buf[i]) : peak;
        if (out)
            fwrite(buf, 4, x.result, out);
        got += x.result;
    }
    if (out)
        fclose(out);
    printf("readi %d frames %lu\n", err, got);
    printf("# peak %d of 32767\n", peak);
    printf("state.running %d\n", state(fd));
    struct snd_pcm_status st;
    memset(&st, 0, sizeof(st));
    r = xi(fd, SNDRV_PCM_IOCTL_STATUS, &st);
    printf("status %d state %d waiting %s\n", r, (int)st.state, st.hw_ptr - st.appl_ptr == st.avail && st.avail < 4800 ? "ok" : "bad");
    printf("# appl %lu hw %lu avail %lu\n", st.appl_ptr, st.hw_ptr, st.avail);
    long delay = -1;
    r = xi(fd, SNDRV_PCM_IOCTL_DELAY, &delay);
    printf("delay %d %s\n", r, delay >= 0 && delay <= 4800 + 64 ? "ok" : "bad");
    /* Asleep for five buffers: the waiting frames reach stop_threshold, the next read is EPIPE. */
    usleep(500000);
    x.frames = 1200;
    printf("readi.overrun %d\n", xi(fd, SNDRV_PCM_IOCTL_READI_FRAMES, &x));
    printf("state.xrun %d\n", state(fd));
    printf("prepare %d\n", xi(fd, SNDRV_PCM_IOCTL_PREPARE, NULL));
    printf("start %d\n", xi(fd, SNDRV_PCM_IOCTL_START, NULL));
    printf("drop %d\n", xi(fd, SNDRV_PCM_IOCTL_DROP, NULL));
    printf("state.dropped %d\n", state(fd));
    unsigned long fr = 5;
    r = xi(fd, SNDRV_PCM_IOCTL_REWIND, &fr);
    printf("rewind %d %lu\n", r, fr);
    printf("write %d\n", write(fd, buf, 4) < 0 ? -errno : 0);
    printf("hw_free %d\n", xi(fd, SNDRV_PCM_IOCTL_HW_FREE, NULL));
    printf("state.freed %d\n", state(fd));
    close(fd);
    free(buf);
    return got == want ? 0 : 1;
}

static int selftest(void);

int main(int argc, char **argv) {
    if (argc < 2)
        return usage();
    if (strcmp(argv[1], "cobalt") == 0)
        return cobalt(argc, argv);
    if (strcmp(argv[1], "capture") == 0)
        return capture(argc, argv);
    if (strcmp(argv[1], "state") == 0)
        return argc == 2 ? show_state() : usage();
    if (strcmp(argv[1], "selftest") == 0)
        return argc == 2 ? selftest() : usage();
    return elem(argc, argv);
}

/* "alsa-hw selftest" (SN-4): control and PCM ioctls, then 1 s of the "hw" signal. Audible. */
static int selftest(void) {
    int c = open("/dev/snd/controlC0", O_RDWR);
    if (c < 0) {
        printf("open controlC0 %d\n", -errno);
        return 1;
    }
    int v = 0;
    int r = xi(c, SNDRV_CTL_IOCTL_PVERSION, &v);
    printf("ctl.pversion %d %#x\n", r, v);
    struct snd_ctl_card_info ci;
    r = xi(c, SNDRV_CTL_IOCTL_CARD_INFO, &ci);
    printf("ctl.card_info %d %s %s\n", r, ci.driver, ci.id);
    v = -1;
    r = xi(c, SNDRV_CTL_IOCTL_PCM_NEXT_DEVICE, &v);
    printf("ctl.next_device(-1) %d %d\n", r, v);
    r = xi(c, SNDRV_CTL_IOCTL_PCM_NEXT_DEVICE, &v);
    printf("ctl.next_device(0) %d %d\n", r, v);
    close(c);

    /* libasound's open: O_NONBLOCK, then back to blocking with F_SETFL (defaults.pcm.nonblock 1). */
    int fd = open("/dev/snd/pcmC0D0p", O_RDWR | O_NONBLOCK | O_CLOEXEC);
    if (fd < 0) {
        printf("open pcmC0D0p %d\n", -errno);
        return 1;
    }
    fcntl(fd, F_SETFL, O_RDWR);
    r = xi(fd, SNDRV_PCM_IOCTL_PVERSION, &v);
    printf("pcm.pversion %d %#x\n", r, v);
    struct snd_pcm_info info;
    r = xi(fd, SNDRV_PCM_IOCTL_INFO, &info);
    printf("pcm.info %d %s %u/%u\n", r, (char *)info.name, info.subdevices_avail, info.subdevices_count);
    struct snd_xferi x = {0};
    short *buf = calloc(48000 * 2, sizeof(short));
    x.buf = buf;
    x.frames = 16;
    printf("writei.open %d\n", xi(fd, SNDRV_PCM_IOCTL_WRITEI_FRAMES, &x));
    printf("prepare.open %d\n", xi(fd, SNDRV_PCM_IOCTL_PREPARE, NULL));
    printf("state.open %d\n", state(fd));

    struct snd_pcm_hw_params p;
    any(&p);
    r = xi(fd, SNDRV_PCM_IOCTL_HW_REFINE, &p);
    printf("refine %d access %#x format %#x channels %u-%u rate %u-%u sample_bits %u-%u buffer_bytes_max %u\n", r,
           p.masks[0].bits[0], p.masks[1].bits[0], iv(&p, SNDRV_PCM_HW_PARAM_CHANNELS)->min, iv(&p, SNDRV_PCM_HW_PARAM_CHANNELS)->max,
           iv(&p, SNDRV_PCM_HW_PARAM_RATE)->min, iv(&p, SNDRV_PCM_HW_PARAM_RATE)->max, iv(&p, SNDRV_PCM_HW_PARAM_SAMPLE_BITS)->min,
           iv(&p, SNDRV_PCM_HW_PARAM_SAMPLE_BITS)->max, iv(&p, SNDRV_PCM_HW_PARAM_BUFFER_BYTES)->max);
    any(&p);
    fix(&p, SNDRV_PCM_HW_PARAM_RATE, 44200);
    printf("refine.44200 %d\n", xi(fd, SNDRV_PCM_IOCTL_HW_REFINE, &p));

    any(&p);
    mask1(&p, SNDRV_PCM_HW_PARAM_ACCESS, SNDRV_PCM_ACCESS_RW_INTERLEAVED);
    mask1(&p, SNDRV_PCM_HW_PARAM_FORMAT, SNDRV_PCM_FORMAT_S16_LE);
    fix(&p, SNDRV_PCM_HW_PARAM_CHANNELS, 2);
    fix(&p, SNDRV_PCM_HW_PARAM_RATE, 48000);
    fix(&p, SNDRV_PCM_HW_PARAM_PERIOD_SIZE, 1200);
    fix(&p, SNDRV_PCM_HW_PARAM_BUFFER_SIZE, 4800);
    r = xi(fd, SNDRV_PCM_IOCTL_HW_PARAMS, &p);
    printf("hw_params %d period %u periods %u buffer %u bytes %u msbits %u rate %u/%u pause %d mmap %d\n", r,
           iv(&p, SNDRV_PCM_HW_PARAM_PERIOD_SIZE)->min, iv(&p, SNDRV_PCM_HW_PARAM_PERIODS)->min, iv(&p, SNDRV_PCM_HW_PARAM_BUFFER_SIZE)->min,
           iv(&p, SNDRV_PCM_HW_PARAM_BUFFER_BYTES)->min, p.msbits, p.rate_num, p.rate_den, !!(p.info & SNDRV_PCM_INFO_PAUSE),
           !!(p.info & SNDRV_PCM_INFO_MMAP));
    printf("state.setup %d\n", state(fd));
    printf("writei.setup %d\n", xi(fd, SNDRV_PCM_IOCTL_WRITEI_FRAMES, &x));

    struct snd_pcm_sw_params sw;
    memset(&sw, 0, sizeof(sw));
    sw.avail_min = 0;
    sw.proto = SNDRV_PCM_VERSION;
    printf("sw_params.avail_min0 %d\n", xi(fd, SNDRV_PCM_IOCTL_SW_PARAMS, &sw));
    sw.avail_min = 1200;
    sw.start_threshold = 4800;
    sw.stop_threshold = 4800;
    r = xi(fd, SNDRV_PCM_IOCTL_SW_PARAMS, &sw);
    printf("sw_params %d boundary %s\n", r, sw.boundary >= 4800ul << 40 && sw.boundary % 4800 == 0 ? "ok" : "bad");

    printf("prepare %d\n", xi(fd, SNDRV_PCM_IOCTL_PREPARE, NULL));
    printf("state.prepared %d\n", state(fd));
    printf("start.empty %d\n", xi(fd, SNDRV_PCM_IOCTL_START, NULL));
    printf("pause.prepared %d\n", ioctl(fd, SNDRV_PCM_IOCTL_PAUSE, 1) < 0 ? -errno : 0);
    printf("resume %d\n", xi(fd, SNDRV_PCM_IOCTL_RESUME, NULL));
    x.buf = (void *)16;
    x.frames = 100;
    printf("writei.efault %d\n", xi(fd, SNDRV_PCM_IOCTL_WRITEI_FRAMES, &x));

    for (unsigned n = 0; n < 48000; n++)
        for (int ch = 0; ch < 2; ch++)
            buf[n * 2 + ch] = sig(n, ch);
    x.buf = buf;
    x.frames = 48000;
    r = xi(fd, SNDRV_PCM_IOCTL_WRITEI_FRAMES, &x);
    printf("writei %d result %ld\n", r, (long)x.result);
    printf("state.running %d\n", state(fd));
    long delay = -1;
    r = xi(fd, SNDRV_PCM_IOCTL_DELAY, &delay);
    printf("delay %d %s\n", r, delay > 0 && delay <= 4800 + 4800 ? "ok" : "bad");
    struct snd_pcm_sync_ptr sp;
    memset(&sp, 0, sizeof(sp));
    sp.flags = SNDRV_PCM_SYNC_PTR_HWSYNC | SNDRV_PCM_SYNC_PTR_APPL | SNDRV_PCM_SYNC_PTR_AVAIL_MIN;
    r = xi(fd, SNDRV_PCM_IOCTL_SYNC_PTR, &sp);
    printf("sync_ptr %d state %d appl %lu avail_min %lu\n", r, (int)sp.s.status.state, sp.c.control.appl_ptr, sp.c.control.avail_min);
    printf("drain %d\n", xi(fd, SNDRV_PCM_IOCTL_DRAIN, NULL));
    struct snd_pcm_status st;
    memset(&st, 0, sizeof(st));
    r = xi(fd, SNDRV_PCM_IOCTL_STATUS, &st);
    printf("status.drained %d state %d appl %lu hw %lu avail %lu delay %ld\n", r, (int)st.state, st.appl_ptr, st.hw_ptr, st.avail, (long)st.delay);
    unsigned long fr = 5;
    r = xi(fd, SNDRV_PCM_IOCTL_REWIND, &fr);
    printf("rewind %d %lu\n", r, fr);
    printf("hw_free %d\n", xi(fd, SNDRV_PCM_IOCTL_HW_FREE, NULL));
    printf("state.freed %d\n", state(fd));
    printf("ioctl.unknown %d\n", xi(fd, _IO('A', 0x7f), NULL));
    close(fd);
    free(buf);
    return 0;
}
