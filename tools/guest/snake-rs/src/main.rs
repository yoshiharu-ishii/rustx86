//! snake — シリアル端末で動く古典スネーク (rustx86のLinuxゲスト用)。
//!
//! 依存ゼロ・std直書き。端末の生モードは termios を ioctl で直接叩き、
//! 時間待ちも nanosleep を int 0x80 で直接呼ぶ (浮動小数点を一切使わない —
//! rustx86 の x87 は検出専用の最小実装なので、FPを避ける)。
//! ビルド: i586-unknown-linux-musl、SSE無効 (.cargo/config.toml 参照)。

use std::io::{Read, Write};

const W: usize = 40;
const H: usize = 20;

const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
const ICANON: u32 = 0o000002;
const ECHO: u32 = 0o000010;

#[repr(C)]
#[derive(Clone, Copy)]
struct Termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 32],
    c_ispeed: u32,
    c_ospeed: u32,
}

fn ioctl(fd: usize, req: usize, arg: usize) -> isize {
    let ret: isize;
    unsafe {
        std::arch::asm!(
            "int 0x80",
            inlateout("eax") 54usize => ret,
            in("ebx") fd, in("ecx") req, in("edx") arg,
        );
    }
    ret
}

#[repr(C)]
struct Timespec {
    tv_sec: isize,
    tv_nsec: isize,
}

fn sleep_ms(ms: isize) {
    // nanosleep(&req, NULL) を int 0x80 で直接 (Durationの浮動小数点を避ける)
    let req = Timespec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    unsafe {
        std::arch::asm!(
            "int 0x80",
            in("eax") 162usize, // nanosleep
            in("ebx") &req as *const _ as usize,
            in("ecx") 0usize,
            lateout("eax") _,
        );
    }
}

fn read_key() -> Option<u8> {
    let mut buf = [0u8; 1];
    match std::io::stdin().read(&mut buf) {
        Ok(1) => Some(buf[0]),
        _ => None,
    }
}

struct Rng(u32);
impl Rng {
    fn next(&mut self, m: usize) -> usize {
        self.0 = self.0.wrapping_mul(1103515245).wrapping_add(12345);
        ((self.0 >> 16) as usize) % m
    }
}

fn main() {
    let mut saved = Termios {
        c_iflag: 0, c_oflag: 0, c_cflag: 0, c_lflag: 0,
        c_line: 0, c_cc: [0; 32], c_ispeed: 0, c_ospeed: 0,
    };
    ioctl(0, TCGETS, &mut saved as *mut _ as usize);
    let mut raw = saved;
    raw.c_lflag &= !(ICANON | ECHO);
    raw.c_cc[6] = 0; // VMIN
    raw.c_cc[5] = 0; // VTIME
    ioctl(0, TCSETS, &raw as *const _ as usize);

    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[2J\x1b[?25l");

    let mut snake: Vec<(usize, usize)> = (0..3).map(|i| (W / 2 - i, H / 2)).collect();
    let (mut dx, mut dy): (isize, isize) = (1, 0);
    let mut rng = Rng(0x5EED_CAFE);
    let mut food = (rng.next(W - 2) + 1, rng.next(H - 2) + 1);
    let mut score = 0u32;

    'game: loop {
        let mut grid = [[b' '; W]; H];
        grid[food.1][food.0] = b'*';
        for (i, &(x, y)) in snake.iter().enumerate() {
            grid[y][x] = if i == 0 { b'@' } else { b'o' };
        }
        let mut frame = String::from("\x1b[H\x1b[1m+");
        frame.push_str(&"-".repeat(W));
        frame.push_str(&format!("+\x1b[0m  score {score}\r\n"));
        for row in &grid {
            frame.push_str("\x1b[1m|\x1b[0m");
            frame.push_str(std::str::from_utf8(row).unwrap());
            frame.push_str("\x1b[1m|\x1b[0m\r\n");
        }
        frame.push_str("\x1b[1m+");
        frame.push_str(&"-".repeat(W));
        frame.push_str("+\x1b[0m  hjkl/wasd で移動、q で終了\r\n");
        let _ = out.write_all(frame.as_bytes());
        let _ = out.flush();

        sleep_ms(120);
        while let Some(c) = read_key() {
            match c {
                b'q' => break 'game,
                b'h' | b'a' if dx != 1 => (dx, dy) = (-1, 0),
                b'l' | b'd' if dx != -1 => (dx, dy) = (1, 0),
                b'k' | b'w' if dy != 1 => (dx, dy) = (0, -1),
                b'j' | b's' if dy != -1 => (dx, dy) = (0, 1),
                _ => {}
            }
        }

        let head = snake[0];
        let nx = head.0 as isize + dx;
        let ny = head.1 as isize + dy;
        if nx < 0 || nx >= W as isize || ny < 0 || ny >= H as isize {
            break;
        }
        let new_head = (nx as usize, ny as usize);
        if snake.contains(&new_head) {
            break;
        }
        snake.insert(0, new_head);
        if new_head == food {
            score += 10;
            food = (rng.next(W - 2) + 1, rng.next(H - 2) + 1);
        } else {
            snake.pop();
        }
    }

    let _ = write!(
        out,
        "\x1b[{};{}H\x1b[1;31m GAME OVER — score {score} \x1b[0m\x1b[?25h\r\n",
        H / 2 + 1,
        W / 2 - 8
    );
    let _ = out.flush();
    ioctl(0, TCSETS, &saved as *const _ as usize);
}
