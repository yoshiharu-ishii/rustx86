/* snake — シリアル端末で動く古典スネーク。
 *
 * rustx86 の Linux ゲスト用に、依存を libc だけに絞った1ファイル。
 * 画面は ANSI エスケープ、入力は termios の生モード。
 * ビルド (i386静的): gcc -m32 -static -O2 -o snake snake.c
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <termios.h>
#include <sys/select.h>
#include <time.h>

#define W 40
#define H 20
#define MAXLEN (W * H)

static struct termios saved;

static void restore(void) {
    tcsetattr(0, TCSANOW, &saved);
    printf("\x1b[?25h\x1b[0m\x1b[2J\x1b[H");
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

int main(void) {
    int sx[MAXLEN], sy[MAXLEN], len = 3, dx = 1, dy = 0;
    int fx, fy, score = 0, i, c, head = 0;
    unsigned seed = (unsigned)time(0);

    raw();
    printf("\x1b[2J\x1b[?25l");
    for (i = 0; i < len; i++) { sx[i] = W / 2 - i; sy[i] = H / 2; }
    srand(seed);
    fx = rand() % (W - 2) + 1; fy = rand() % (H - 2) + 1;

    for (;;) {
        /* 枠 */
        printf("\x1b[H\x1b[1m+");
        for (i = 0; i < W; i++) putchar('-');
        printf("+\x1b[0m  score %d\r\n", score);
        {
            static char grid[H][W + 1];
            int r;
            memset(grid, ' ', sizeof grid);
            grid[fy][fx] = '*';
            for (i = 0; i < len; i++)
                grid[sy[i]][sx[i]] = i ? 'o' : '@';
            for (r = 0; r < H; r++) {
                grid[r][W] = 0;
                printf("\x1b[1m|\x1b[0m%s\x1b[1m|\x1b[0m\r\n", grid[r]);
            }
        }
        printf("\x1b[1m+");
        for (i = 0; i < W; i++) putchar('-');
        printf("+\x1b[0m  hjkl/wasd で移動、q で終了\r\n");
        fflush(stdout);

        usleep(120000);
        c = key();
        if (c == 'q') break;
        if ((c == 'h' || c == 'a') && dx != 1) { dx = -1; dy = 0; }
        if ((c == 'l' || c == 'd') && dx != -1) { dx = 1; dy = 0; }
        if ((c == 'k' || c == 'w') && dy != 1) { dx = 0; dy = -1; }
        if ((c == 'j' || c == 's') && dy != -1) { dx = 0; dy = 1; }

        /* 進む: 尾から詰める */
        for (i = len - 1; i > 0; i--) { sx[i] = sx[i - 1]; sy[i] = sy[i - 1]; }
        sx[0] += dx; sy[0] += dy;

        /* 壁と自分 */
        if (sx[0] < 0 || sx[0] >= W || sy[0] < 0 || sy[0] >= H) break;
        for (i = 1; i < len; i++)
            if (sx[i] == sx[0] && sy[i] == sy[0]) goto dead;

        if (sx[0] == fx && sy[0] == fy) {
            score += 10;
            if (len < MAXLEN) len++;
            sx[len - 1] = sx[len - 2]; sy[len - 1] = sy[len - 2];
            fx = rand() % (W - 2) + 1; fy = rand() % (H - 2) + 1;
        }
    }
dead:
    printf("\x1b[%d;%dH\x1b[1;31m GAME OVER — score %d \x1b[0m\r\n", H / 2 + 1, W / 2 - 8, score);
    return 0;
}
