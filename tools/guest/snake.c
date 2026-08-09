/* snake — シリアル端末で動く古典スネーク。
 *
 * rustx86 の Linux ゲスト用に、依存を libc だけに絞った1ファイル。
 *
 * ## 描画は差分だけ
 *
 * シリアルは細い管である (エミュレータなら尚更)。毎フレーム全画面を
 * 流すと1.4KB×毎tickで管が詰まるので、**動いたマスだけ**書く:
 * 蛇の頭 (書く)・尻尾 (消す)・餌 (出たとき) — 1フレーム数十バイトで済む。
 * 枠は起動時に1回だけ描く。80年代のBBSドアゲームと同じ設計である。
 *
 * ビルド (i386静的): gcc -m32 -static -O2 -o snake snake.c
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <termios.h>
#include <sys/select.h>
#include <time.h>

#define W 40
#define H 20
#define MAXLEN (W * H)

static struct termios saved;

static void restore(void) {
    tcsetattr(0, TCSANOW, &saved);
    printf("\x1b[?25h\x1b[0m\x1b[%d;1H\n", H + 3);
    fflush(stdout);
}

static void raw(void) {
    struct termios t;
    tcgetattr(0, &saved);
    atexit(restore);
    t = saved;
    t.c_lflag &= ~(ICANON | ECHO);
    t.c_cc[VMIN] = 0;
    t.c_cc[VTIME] = 0;
    tcsetattr(0, TCSANOW, &t);
}

static int key(void) {
    unsigned char c;
    fd_set fds;
    struct timeval tv = {0, 0};
    FD_ZERO(&fds);
    FD_SET(0, &fds);
    if (select(1, &fds, 0, 0, &tv) > 0 && read(0, &c, 1) == 1) return c;
    return -1;
}

/* 盤面座標 (0..W-1, 0..H-1) のマスに1文字置く。枠のぶん +2/+2 ずらす */
static void put(int x, int y, char c) {
    printf("\x1b[%d;%dH%c", y + 2, x + 2, c);
}

int main(void) {
    int sx[MAXLEN], sy[MAXLEN], len = 3, dx = 1, dy = 0;
    int fx, fy, score = 0, i, c;

    raw();
    /* 枠は1回だけ描く */
    printf("\x1b[2J\x1b[?25l\x1b[H+");
    for (i = 0; i < W; i++) putchar('-');
    printf("+  score 0\r\n");
    for (i = 0; i < H; i++) {
        printf("|\x1b[%d;%dH|\r\n", i + 2, W + 2);
    }
    printf("+");
    for (i = 0; i < W; i++) putchar('-');
    printf("+  hjkl/wasd で移動、q で終了");

    for (i = 0; i < len; i++) { sx[i] = W / 2 - i; sy[i] = H / 2; }
    for (i = 0; i < len; i++) put(sx[i], sy[i], i ? 'o' : '@');
    srand((unsigned)time(0));
    fx = rand() % W; fy = rand() % H;
    put(fx, fy, '*');
    fflush(stdout);

    for (;;) {
        struct timespec ts = {0, 120 * 1000 * 1000};
        nanosleep(&ts, 0);
        c = key();
        if (c == 'q') break;
        if ((c == 'h' || c == 'a') && dx != 1) { dx = -1; dy = 0; }
        if ((c == 'l' || c == 'd') && dx != -1) { dx = 1; dy = 0; }
        if ((c == 'k' || c == 'w') && dy != 1) { dx = 0; dy = -1; }
        if ((c == 'j' || c == 's') && dy != -1) { dx = 0; dy = 1; }

        int nx = sx[0] + dx, ny = sy[0] + dy;
        if (nx < 0 || nx >= W || ny < 0 || ny >= H) break;
        for (i = 1; i < len; i++)
            if (sx[i] == nx && sy[i] == ny) goto dead;

        /* 差分: 旧頭を胴に、新頭を描き、伸びないなら尻尾を消す */
        put(sx[0], sy[0], 'o');
        if (nx == fx && ny == fy) {
            score += 10;
            if (len < MAXLEN) len++;
            printf("\x1b[1;%dH%-6d", W + 11, score);
            fx = rand() % W; fy = rand() % H;
            put(fx, fy, '*');
        } else {
            put(sx[len - 1], sy[len - 1], ' ');
        }
        for (i = len - 1; i > 0; i--) { sx[i] = sx[i - 1]; sy[i] = sy[i - 1]; }
        sx[0] = nx; sy[0] = ny;
        put(nx, ny, '@');
        fflush(stdout);
    }
dead:
    printf("\x1b[%d;%dH GAME OVER — score %d ", H / 2 + 1, W / 2 - 8, score);
    return 0;
}
