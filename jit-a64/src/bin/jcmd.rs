//! jcmd — 定常ワークロード (gcc等) でJIT on/offを比べる定規。
//!
//!   DISK=images/disk-gcc.img cargo run --release -p rustx86-jit-a64 --bin jcmd
//!   RUSTX86_JIT=0 DISK=... (off側)
//!
//! guestcmd (core/examples) の写し + JIT取り付け + **コマンド窓の計測**:
//! シェル到達でコマンドを流し、DONEMARKまでの命令数と壁時計を測る。
//! ブートは分岐だらけでJITに不利な土俵 — コンパイルのような定常ループで
//! ブロックが伸びるかを、この窓の実効MIPSで裁く (ADR-0013の「定常WL」)。
//! initrdは既定で**現行のinitramfs-mini** (virtio/squashfsモジュールが要る —
//! 凍結bench対にはまだ入っていない)

use rustx86_core::{initrd_ram_needed, Machine, MachineProfile};

fn fnv(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn main() {
    let kernel = std::env::var("KERNEL").unwrap_or_else(|_| "images/vmlinuz-lts".into());
    let data = std::fs::read(&kernel).unwrap_or_else(|e| panic!("{kernel}: {e}"));
    let initrd_path = std::env::var("INITRD").unwrap_or_else(|_| "images/initramfs-mini".into());
    let initrd = std::fs::read(&initrd_path).unwrap_or_else(|e| panic!("{initrd_path}: {e}"));
    let disk = std::env::var("DISK")
        .ok()
        .map(|p| std::fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}")));
    let cmd = std::env::var("GUEST_CMD").unwrap_or_else(|_| {
        // 既定: gccを2回 (冷間=ディスク読み込み込み、温間=ページキャッシュ済み)
        "time gcc /hello.c -o /tmp/h1; time gcc /hello.c -o /tmp/h2; printf \"DONE%s\\n\" MARK"
            .into()
    });
    let budget: u64 = 120_000_000_000;
    let mb: usize = {
        let need = initrd_ram_needed(&initrd);
        ((need + need / 4).div_ceil(64 << 20) as usize * 64).max(128)
    };

    let mut m = Box::new(Machine::with_profile(MachineProfile::pc_32bit(mb)));
    if let Some(img) = disk {
        m.blk_attach(img);
    }
    if let Err(e) = m.boot_linux_with_initrd(&data, "console=ttyS0", Some(&initrd)) {
        eprintln!("起動できない: {e}");
        std::process::exit(1);
    }
    let jit_on = unsafe { rustx86_jit_a64::attach_if_enabled(&mut m) };
    eprintln!(
        "[jcmd] JIT: {} / RAM {}MB / cmd: {}",
        if jit_on { "on" } else { "off" },
        mb,
        cmd
    );

    let t0 = std::time::Instant::now();
    // opstats: 窓のオペコード度数 (feed時点のスナップショットとの差分)
    #[allow(unused_mut, unused_variables)]
    let mut win_ops: Vec<u64> = Vec::new();
    let (mut fed, mut done_at) = (false, None::<usize>);
    let mut spent: u64 = 0;
    // コマンド窓の計測 (feed時点からDONEMARKまで)
    let mut win_start: Option<(u64, std::time::Instant, u64, u64)> = None; // (spent, wall, jit_instrs, jit_entries)
    while spent < budget {
        m.run(2_000_000);
        spent += 2_000_000;
        let out = String::from_utf8_lossy(&m.devices.uart.tx);
        if !fed && out.contains("busybox shell") {
            for b in format!("{cmd}\n").bytes() {
                m.devices.uart.rx.push_back(b);
            }
            fed = true;
            win_start = Some((
                spent,
                std::time::Instant::now(),
                m.jit_instrs,
                m.jit_entries,
            ));
            if cfg!(feature = "opstats") {
                win_ops = m.op_counts.clone();
            }
        }
        if fed && done_at.is_none() {
            if let Some(at) = out.rfind("DONEMARK") {
                done_at = Some(at);
            }
        }
        if done_at.is_some() {
            m.run(10_000_000);
            break;
        }
    }
    if done_at.is_none() {
        eprintln!(
            "[jcmd] 予算内に目印が来なかった (spent={}G, halted={})",
            spent / 1_000_000_000,
            m.halted
        );
        eprintln!("--- シリアルの尻 (死因の現場) ---");
        let tx = &m.devices.uart.tx;
        let tail = &tx[tx.len().saturating_sub(2000)..];
        eprintln!("{}", String::from_utf8_lossy(tail));
        std::process::exit(2);
    }
    // ゲストのtime出力ごとシリアルを出す (実測の原本)
    print!("{}", String::from_utf8_lossy(&m.devices.uart.tx));
    let (s0, w0, j0, e0) = win_start.unwrap();
    let win_instr = spent - s0;
    let win_wall = w0.elapsed().as_secs_f64();
    println!("\n[jcmd] JIT: {}", if jit_on { "on" } else { "off" });
    println!(
        "[jcmd] コマンド窓: {}M命令 / {:.2}s = {:.1} MIPS (起動込み全体 {:.1}s)",
        win_instr / 1_000_000,
        win_wall,
        win_instr as f64 / 1e6 / win_wall,
        t0.elapsed().as_secs_f64()
    );
    println!(
        "[jcmd] 指紋: 窓命令数={} シリアルFNV={:016x}",
        win_instr,
        fnv(&m.devices.uart.tx)
    );
    if cfg!(feature = "opstats") && !win_ops.is_empty() {
        let delta: Vec<u64> = m
            .op_counts
            .iter()
            .zip(&win_ops)
            .map(|(a, b)| a - b)
            .collect();
        let total: u64 = delta.iter().sum();
        let mut idx: Vec<usize> = (0..512).collect();
        idx.sort_by_key(|&i| std::cmp::Reverse(delta[i]));
        println!("[jcmd] 窓のオペコード上位 (全{}M命令):", total / 1_000_000);
        let mut cum = 0.0;
        for &i in idx.iter().take(30) {
            let c = delta[i];
            if c == 0 {
                break;
            }
            let pct = c as f64 * 100.0 / total as f64;
            cum += pct;
            let name = if i < 256 {
                format!("{:02X}", i)
            } else {
                format!("0F{:02X}", i - 256)
            };
            println!(
                "  {name:>5}  {:>7}M  {pct:5.1}%  累積{cum:5.1}%",
                c / 1_000_000
            );
        }
    }
    #[cfg(feature = "opstats")]
    if jit_on {
        let miss = rustx86_core::jit::vocab_miss_report();
        let total: u64 = miss.iter().map(|&(_, c)| c).sum();
        println!("[jcmd] collectを止めたuop上位 (全{}k回):", total / 1000);
        for &(name, c) in miss.iter().take(15) {
            println!(
                "  {name:<28} {:>6}k  {:4.1}%",
                c / 1000,
                c as f64 * 100.0 / total as f64
            );
        }
    }
    if jit_on {
        let (baked, rejected, installed, demoted) = rustx86_jit_a64::stats();
        println!(
            "[jcmd] jit: 窓内実行{}M命令 (窓カバレッジ{:.1}%) / 窓内入場{}M回 (平均{:.1}命令) / 焼き{} 棄却{} 据付{} 降格{}",
            (m.jit_instrs - j0) / 1_000_000,
            (m.jit_instrs - j0) as f64 * 100.0 / win_instr as f64,
            (m.jit_entries - e0) / 1_000_000,
            (m.jit_instrs - j0) as f64 / (m.jit_entries - e0).max(1) as f64,
            baked,
            rejected,
            installed,
            demoted
        );
    }
}
