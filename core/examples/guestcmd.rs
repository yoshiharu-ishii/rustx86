//! ゲストで1コマンド走らせて出力を持ち帰る非対話ランナー。
//!
//! ```bash
//! GUEST_CMD='ls /; printf "DONE%s\n" MARK' cargo run --release --example guestcmd
//! DISK=disk.img GUEST_CMD='md5sum /dev/vda; printf "DONE%s\n" MARK' cargo run --release --example guestcmd
//! ```
//!
//! [`run`](./run.rs) は人間用 (Ctrl-]が来るまで終わらない) なので、検証を
//! `run </dev/null` で流すと**永遠に回り続ける** (実際に48分回した)。
//! 自動検証はこちらを使う — シェルの到着を待ってコマンドを流し、
//! `DONEMARK` を見たら降りる。**必ず終わる**のが仕事:
//!
//! - 目印はコマンド側が `printf "DONE%s\n" MARK` で出す (直書きすると
//!   エコーバックが目印に見えて早降りする — printfの連結で回避)
//! - 押しても目印が来なければ命令予算で降りる (無限に待たない)
//!
//! 環境変数: KERNEL / INITRD / DISK / RAM_MB / CMDLINE / GUEST_CMD / BUDGET_G (10^9命令)

use rustx86_core::{initrd_ram_needed, Machine, MachineProfile};

fn main() {
    let kernel = std::env::var("KERNEL").unwrap_or_else(|_| "images/vmlinuz-lts".into());
    let data = std::fs::read(&kernel).unwrap_or_else(|e| panic!("{kernel}: {e}"));
    let initrd_path = std::env::var("INITRD").unwrap_or_else(|_| "images/initramfs-mini".into());
    let initrd = std::fs::read(&initrd_path).unwrap_or_else(|e| panic!("{initrd_path}: {e}"));
    let disk = std::env::var("DISK")
        .ok()
        .map(|p| std::fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}")));
    let cmdline = std::env::var("CMDLINE").unwrap_or_else(|_| "console=ttyS0".into());
    let cmd = std::env::var("GUEST_CMD").unwrap_or_else(|_| "printf \"DONE%s\\n\" MARK".into());
    let budget: u64 = std::env::var("BUDGET_G")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60) // 60G命令 ≒ フル起動+コンパイル数回ぶん
        * 1_000_000_000;

    // RAMはrun.rsと同じ自動判定 (下限+25%を64MB刻みで切り上げ)
    let mb: usize = std::env::var("RAM_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            let need = initrd_ram_needed(&initrd);
            ((need + need / 4).div_ceil(64 << 20) as usize * 64).max(128)
        });

    let mut m = Machine::with_profile(MachineProfile::pc_32bit(mb));
    if let Some(img) = disk {
        m.blk_attach(img);
    }
    if let Err(e) = m.boot_linux_with_initrd(&data, &cmdline, Some(&initrd)) {
        eprintln!("起動できない: {e}");
        std::process::exit(1);
    }

    let (mut fed, mut done_at) = (false, None::<usize>);
    let mut spent: u64 = 0;
    let mut printed = 0usize; // シリアルをここまで流した (貯めて最後に吐かない —
                              // 外の番犬が「生きて進んでいるか」を読めるように)
    let mut beat: u64 = 0;
    while spent < budget {
        m.run(2_000_000);
        spent += 2_000_000;
        // 心拍: 5G命令ごとにstderrへ1行。ゲストが黙る区間 (コンパイル中など)
        // でも、機械が回っている限りこれは打たれ続ける
        if spent / 5_000_000_000 > beat {
            beat = spent / 5_000_000_000;
            eprintln!("[guestcmd] {}G命令 経過", beat * 5);
        }
        {
            use std::io::Write;
            let tx = &m.devices.uart.tx;
            if tx.len() > printed {
                let mut out = std::io::stdout();
                let _ = out.write_all(&tx[printed..]);
                let _ = out.flush();
                printed = tx.len();
            }
        }
        let out = String::from_utf8_lossy(&m.devices.uart.tx);
        if !fed && out.contains("busybox shell") {
            for b in format!("{cmd}\n").bytes() {
                m.devices.uart.rx.push_back(b);
            }
            fed = true;
        }
        if fed && done_at.is_none() {
            if let Some(at) = out.rfind("DONEMARK") {
                done_at = Some(at);
            }
        }
        // 目印の後のプロンプトまで少し回してから降りる (出力の尻切れ防止)
        if done_at.is_some() {
            m.run(10_000_000);
            break;
        }
    }
    {
        use std::io::Write;
        let tx = &m.devices.uart.tx;
        if tx.len() > printed {
            let mut out = std::io::stdout();
            let _ = out.write_all(&tx[printed..]);
            let _ = out.flush();
        }
    }
    if done_at.is_none() {
        eprintln!(
            "\n[guestcmd] 予算 {}G 命令で目印が来なかった",
            budget / 1_000_000_000
        );
        std::process::exit(2);
    }
}
