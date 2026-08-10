//! 対話ランナー — シリアルコンソールをそのままターミナルに繋ぐ。
//!
//! ```bash
//! cargo run --release --example run -- images/vmlinuz-lts
//! ```
//!
//! bzImage (+同じ場所の initramfs-lts) を起動し、UARTの出力を標準出力へ、
//! キー入力をUARTへ流す。**Linuxのシェルにネイティブで触るための最短の道**。
//! 終了は Ctrl-] (telnet の作法)。
//!
//! 生モード (1キーずつ・エコーなし) は `stty` に頼る — termios のために
//! 依存を増やさない (coreの無依存はexamplesにも波及させない)。

use rustx86_core::{cpu, Machine, MachineProfile};
use std::io::{Read, Write};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "images/vmlinuz-lts".into());
    let data = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let initrd_path = std::env::var("INITRD").unwrap_or_else(|_| {
        std::path::Path::new(&path)
            .with_file_name("initramfs-lts")
            .to_string_lossy()
            .into_owned()
    });
    let initrd = std::fs::read(&initrd_path).ok();
    let cmdline = std::env::var("CMDLINE").unwrap_or_else(|_| "console=ttyS0".into());

    let mb: usize = std::env::var("RAM_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(128);
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(mb));
    // bzImage / vmlinux は中身で自動判別。vmlinux (tools/extract-vmlinux.sh) なら
    // 自己解凍ステブが無いぶん起動が4割速い
    m.boot_linux_with_initrd(&data, &cmdline, initrd.as_deref())
        .expect("boot");
    eprintln!(
        "rustx86: {path} ({}MB, initrd {}) — Ctrl-] で終了",
        mb,
        initrd
            .as_ref()
            .map_or("なし".into(), |d| format!("{}K", d.len() / 1024)),
    );

    // 標準入力を生モードに。終了時に戻す
    let _ = std::process::Command::new("stty")
        .args(["raw", "-echo"])
        .stdin(std::process::Stdio::inherit())
        .status();

    // 入力は別スレッドで読んでチャネルへ (標準入力のブロッキングを避ける)
    let (tx, rx) = std::sync::mpsc::channel::<u8>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1];
        while stdin.read_exact(&mut buf).is_ok() {
            if tx.send(buf[0]).is_err() {
                break;
            }
        }
    });

    let mut printed = 0usize;
    let mut out = std::io::stdout();
    'outer: loop {
        // まとめて回す (1命令ごとのオーバーヘッドを避ける)。
        // 刻みは20万命令 ≒ 数ms — **これより粗いと打鍵の間隔がつぶれる**。
        // Escと次のキーが同じバッチで届くと、viがエスケープシーケンスの
        // 断片と解釈して両方を飲み込む (実機のシリアルはキー間に必ず隙間がある)
        //
        // 予算は**仮想時間 (TSCの進み)** で数える。アイドル (HLT) の早送りが
        // 入ったので、暇なときは数回のstepで20万命令ぶんの時間が流れる
        let start_tsc = m.cpu.tsc;
        while m.cpu.tsc.wrapping_sub(start_tsc) < 200_000 {
            m.step();
            if m.trap.is_some() {
                break;
            }
        }
        // 出力を流す
        if m.devices.uart.tx.len() > printed {
            let chunk = &m.devices.uart.tx[printed..];
            let _ = out.write_all(chunk);
            let _ = out.flush();
            printed = m.devices.uart.tx.len();
        }
        // 入力を移す
        while let Ok(b) = rx.try_recv() {
            if b == 0x1D {
                // Ctrl-]
                break 'outer;
            }
            m.devices.uart.feed(&[b]);
        }
        if let Some(t) = &m.trap {
            eprintln!("\r\n[TRAP: {} at {:04x}:{:08x}]", t.reason, t.cs, t.ip);
            break;
        }
        if m.halted && !m.cpu.flag(cpu::IF) {
            eprintln!("\r\n[DEAD HALT at {:08x}]", m.cpu.ip);
            break;
        }
        // アイドル (HLT) の早送りが飛ばした仮想時間ぶんだけ、実時間で待つ。
        // 待たずに回すとゲストの時計だけが実時間の何百倍も速く進む
        // (snakeが目で追えなくなる)。忙しい実行は1命令も飛ばさないので
        // 待ちゼロ = 全力で回る。換算は 1ゲスト秒 ≒ 1.193182MHz × 64命令
        let skipped = std::mem::take(&mut m.idle_skipped);
        if skipped > 0 {
            std::thread::sleep(std::time::Duration::from_micros(skipped * 1000 / 76_364));
        }
    }

    let _ = std::process::Command::new("stty")
        .args(["sane"])
        .stdin(std::process::Stdio::inherit())
        .status();
    eprintln!("bye");
}
