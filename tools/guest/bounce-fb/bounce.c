/* bounce — Linux の fbdev (/dev/fb0) で色とりどりのボールが跳ね回る。
 *
 *   gcc -static -O2 -o bounce bounce.c      (i386 musl で静的に)
 *
 * DOS版 (tools/guest/bounce/bounce.asm) の Linux 版。やっていることは同じで、
 * 違うのは画素の置き場所の見つけ方だけ:
 *   DOS   : INT 10h で mode 13h、0xA0000 に直書き、0x3DA で垂直帰線を待つ
 *   Linux : /dev/fb0 を open、ioctl で解像度と画素形式を聞き、mmap して書く。
 *           帰線の合図は無いので nanosleep で 70Hz を刻む
 * 画素形式 (bpp・R/G/B の位置) は ioctl が返す var から組み立てる —
 * 決め打ちしない (busybox の fbsplash は 24bpp を BGR 決め打ちで書くので、
 * 赤が下位の rustx86 では色が入れ替わる。そうならないように)。
 * 何かキーを押すと終わる (端末を raw にして poll で覗く)。
 */
#include <fcntl.h>
#include <linux/fb.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

#define NBALLS 8
#define R 6 /* 半径 */

static struct fb_var_screeninfo var;
static struct fb_fix_screeninfo fix;
static uint8_t *fb;

/* (r,g,b) を var の並びで画素値にする */
static uint32_t pix(int r, int g, int b) {
    return ((uint32_t)r >> (8 - var.red.length)) << var.red.offset |
           ((uint32_t)g >> (8 - var.green.length)) << var.green.offset |
           ((uint32_t)b >> (8 - var.blue.length)) << var.blue.offset;
}

static void put(int x, int y, uint32_t v) {
    if (x < 0 || y < 0 || x >= (int)var.xres || y >= (int)var.yres) return;
    uint8_t *p = fb + y * fix.line_length + x * (var.bits_per_pixel / 8);
    for (unsigned i = 0; i < var.bits_per_pixel / 8; i++) p[i] = v >> (8 * i);
}

static void disc(int cx, int cy, uint32_t v) {
    for (int dy = -R; dy <= R; dy++)
        for (int dx = -R; dx <= R; dx++)
            if (dx * dx + dy * dy <= R * R) put(cx + dx, cy + dy, v);
}

struct ball { int x, y, vx, vy; uint32_t col; };

int main(int argc, char **argv) {
    /* 引数にフレーム数があれば、その数だけ回して**絵を残したまま**終わる
     * (回帰テスト用: kill だと「消してから描き直す」の途中に当たり得る) */
    long frames_left = argc > 1 ? atol(argv[1]) : -1;
    int fd = open("/dev/fb0", O_RDWR);
    if (fd < 0) { perror("/dev/fb0"); return 1; }
    if (ioctl(fd, FBIOGET_VSCREENINFO, &var) || ioctl(fd, FBIOGET_FSCREENINFO, &fix)) {
        perror("ioctl"); return 1;
    }
    fb = mmap(NULL, fix.smem_len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (fb == MAP_FAILED) { perror("mmap"); return 1; }
    printf("fb0: %ux%u %ubpp line=%u  r%u@%u g%u@%u b%u@%u\n", var.xres, var.yres,
           var.bits_per_pixel, fix.line_length, var.red.length, var.red.offset,
           var.green.length, var.green.offset, var.blue.length, var.blue.offset);

    /* 端末: エコー無し・1キーずつ。終わったら戻す */
    struct termios old, raw;
    int tty = isatty(0);
    if (tty) { tcgetattr(0, &old); raw = old; raw.c_lflag &= ~(ICANON | ECHO); tcsetattr(0, TCSANOW, &raw); }

    int W = var.xres, H = var.yres;
    uint32_t bg = pix(0, 0, 0), wall = pix(64, 64, 200);
    memset(fb, 0, fix.line_length * H);
    for (int x = 0; x < W; x++) { put(x, 0, wall); put(x, H - 1, wall); }
    for (int y = 0; y < H; y++) { put(0, y, wall); put(W - 1, y, wall); }

    struct ball b[NBALLS] = {
        { 60, 40, 2, 1, pix(255, 120, 0) },   { 200, 30, -1, 2, pix(255, 32, 32) },
        { 120, 150, 3, -1, pix(255, 240, 32) }, { 250, 100, -2, -2, pix(64, 240, 64) },
        { 30, 120, 1, 3, pix(64, 240, 255) },  { 160, 80, -3, 1, pix(80, 120, 255) },
        { 280, 170, 2, -3, pix(200, 64, 255) }, { 90, 60, -1, -1, pix(255, 160, 200) },
    };
    struct timespec frame = { 0, 1000000000L / 70 }; /* 70Hz (mode 13h と同じテンポ) */
    struct pollfd pf = { 0, POLLIN, 0 };
    for (;;) {
        for (int i = 0; i < NBALLS; i++) disc(b[i].x, b[i].y, bg);
        for (int i = 0; i < NBALLS; i++) {
            b[i].x += b[i].vx; b[i].y += b[i].vy;
            if (b[i].x < 1 + R) { b[i].x = 1 + R; b[i].vx = -b[i].vx; }
            if (b[i].x > W - 2 - R) { b[i].x = W - 2 - R; b[i].vx = -b[i].vx; }
            if (b[i].y < 1 + R) { b[i].y = 1 + R; b[i].vy = -b[i].vy; }
            if (b[i].y > H - 2 - R) { b[i].y = H - 2 - R; b[i].vy = -b[i].vy; }
        }
        for (int i = 0; i < NBALLS; i++) disc(b[i].x, b[i].y, b[i].col);
        /* キーが来たら終わる。stdin が /dev/null (EOF) のときは終わらない —
         * 回帰テストは `bounce </dev/null &` で回して外から止める */
        if (poll(&pf, 1, 0) > 0) { char c; if (read(0, &c, 1) == 1) break; }
        if (frames_left > 0 && --frames_left == 0) {
            if (tty) tcsetattr(0, TCSANOW, &old);
            return 0; /* 絵は残す */
        }
        nanosleep(&frame, NULL);
    }
    if (tty) tcsetattr(0, TCSANOW, &old);
    memset(fb, 0, fix.line_length * H);
    munmap(fb, fix.smem_len);
    close(fd);
    return 0;
}
