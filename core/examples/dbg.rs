//! gdb風のデバッガ。
//!
//! ```bash
//! cargo run --release --example dbg -- images/fd14games.img
//! ```
//!
//! ## gdbと違うところ
//!
//! 実機のデバッガに無くて、エミュレータにあるものを足してある。
//!
//! - `goto <n>` — **n命令目に戻る**。決定的なので最初から流し直せば必ず同じ状態になる
//! - `until <文字列>` — **画面にその文字が出るまで**走らせる。OSの起動を追う単位はこれ
//! - `save` / `load` — スナップショット。分岐点で保存して何度でも試せる
//!
//! 逆に**逆アセンブラは無い**。命令は生バイトで出す。デコーダは持っているが
//! 文字列に起こす部分は書いていない。要るようになったら足す。
//!
//! ## なぜ自前で書くのか
//!
//! 本物のgdbを繋ぐ道 (GDB remote serial protocol) もあり、そちらは
//! **記号が使える**という決定的な利点がある。ただし効くのは32bit保護モードで
//! Linuxカーネルを追うようになってからで、gdbの16bit対応は貧弱である。
//! 今要るのは「誰がこの番地を書いたか」で、それはこちらで足りる。

use rustx86_core::{debug::Stop, Machine};
use std::io::{BufRead, Write};

fn main() {
    let img = std::env::args().nth(1);
    let mut m = Machine::new();
    let mut disk: Option<Vec<u8>> = None;
    if let Some(path) = &img {
        let data = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        m.boot_from_disk(data.clone()).expect("boot");
        disk = Some(data);
    }

    println!("rustx86 デバッガ。`help` で一覧、`q` で終了");
    show_where(&m);

    let stdin = std::io::stdin();
    let mut snap: Option<Vec<u8>> = None;
    for line in stdin.lock().lines() {
        let line = line.unwrap_or_default();
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let cmd = it.next().unwrap();
        let rest: Vec<&str> = it.collect();
        let arg1 = rest.first().copied();

        match cmd {
            "q" | "quit" => break,
            "help" | "h" => help(),

            // --- 見張る ---
            "b" | "break" => match arg1.and_then(addr) {
                Some(a) => {
                    m.dbg.break_at(a);
                    println!("ブレークポイント {a:#07x}");
                }
                None => println!("使い方: b 0x7c00  /  b 07c0:0000"),
            },
            "w" | "watch" => match arg1.and_then(addr) {
                Some(a) => {
                    m.dbg.watch_mem(a);
                    println!("書き込み監視 {a:#07x}");
                }
                None => println!("使い方: w 0x450"),
            },
            "wi" => match arg1.and_then(|s| addr(s)).map(|a| a as u16) {
                Some(p) => {
                    let rw = rest.get(1).copied().unwrap_or("w");
                    m.dbg.watch_io(p, rw.contains('r'), rw.contains('w'));
                    println!("I/O監視 ポート{p:#06x} ({rw})");
                }
                None => println!("使い方: wi 0x3d5 [rw]"),
            },
            "d" | "delete" => {
                m.dbg.clear();
                println!("見張りを全部外した");
            }
            "info" => info(&m),

            // --- 走らせる ---
            // 上限を持たせる。**パイプ越しにはCtrl-Cが無い**ので、
            // 際限なく走る `c` は帰ってこない (実際に10分回して気づいた)。
            // boot の例で当てずっぽうの数字を消したのと同じ考えで、
            // 上限は暴走を止める番人として置き、**走った命令数を必ず言う**
            "c" | "continue" => {
                let cap = arg1
                    .and_then(|s| s.replace('_', "").parse().ok())
                    .unwrap_or(GUARD);
                run(&mut m, cap);
            }
            "si" | "s" => {
                let n = arg1.and_then(|s| s.parse().ok()).unwrap_or(1);
                m.dbg.run_for(n);
                run(&mut m, n + 1);
            }
            "until" | "u" => {
                let needle = line.splitn(2, char::is_whitespace).nth(1).unwrap_or("").trim();
                if needle.is_empty() {
                    println!("使い方: until FreeDOS kernel");
                } else {
                    until(&mut m, needle, GUARD);
                }
            }
            "goto" => match arg1.and_then(|s| s.replace('_', "").parse::<u64>().ok()) {
                // **巻き戻し。** 決定的なので、最初から流し直せば必ず同じ状態になる
                Some(n) => {
                    let (code, mem, iow, ior, cap) = (
                        m.dbg.code.clone(),
                        m.dbg.mem_write.clone(),
                        m.dbg.io_write.clone(),
                        m.dbg.io_read.clone(),
                        m.dbg.trace_cap,
                    );
                    m = Machine::new();
                    if let Some(d) = &disk {
                        m.boot_from_disk(d.clone()).unwrap();
                    }
                    m.dbg.record_trace(cap);
                    m.dbg.run_to(n);
                    print!("{n} 命令目まで流し直し中… ");
                    let _ = std::io::stdout().flush();
                    run(&mut m, n + 1);
                    m.dbg.code = code;
                    m.dbg.mem_write = mem;
                    m.dbg.io_write = iow;
                    m.dbg.io_read = ior;
                }
                None => println!("使い方: goto 36000000"),
            },

            // --- 見る ---
            "r" | "reg" => show_where(&m),
            "x" => match arg1.and_then(addr) {
                Some(a) => {
                    let n = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(64);
                    dump(&m, a, n);
                }
                None => println!("使い方: x 0x450 [長さ]"),
            },
            "screen" => println!("{}", m.text_screen_string()),
            "t" | "trace" => {
                let n = arg1.and_then(|s| s.parse().ok()).unwrap_or(16);
                trace(&m, n);
            }
            "record" => {
                let n = arg1.and_then(|s| s.parse().ok()).unwrap_or(256);
                m.dbg.record_trace(n);
                println!("足跡を直近{n}命令ぶん残す");
            }

            // --- 保存 ---
            "save" => {
                snap = Some(m.save_state());
                println!("{} 命令目を保存", m.dbg.instr);
            }
            "load" => match &snap {
                Some(s) => match m.load_state(s) {
                    Ok(()) => show_where(&m),
                    Err(e) => println!("戻せない: {e}"),
                },
                None => println!("まだ save していない"),
            },

            _ => println!("`{cmd}` は知らない。`help` を見る"),
        }
    }
}

/// `c` の既定の上限。FreeDOSがDOSプロンプトに至るまでが約3700万命令なので、
/// その27倍。**当てる数字ではなく、帰ってこなくなるのを防ぐ番人**
const GUARD: u64 = 1_000_000_000;

/// 走らせて、止まった理由を言う
fn run(m: &mut Machine, cap: u64) {
    let mut n = 0u64;
    while n < cap {
        m.step();
        n += 1;
        if m.dbg.stop.is_some() {
            break;
        }
        // 本当に止まった機械なら、いくら回しても何も起きない
        if m.halted && m.pending_irq.is_none() && !m.devices.pit.counters[0].running {
            println!("機械が止まった (HLT)");
            break;
        }
    }
    match m.dbg.take_stop() {
        Some(s) => println!("{}", why(m, &s)),
        // 何にも当たらずに上限へ来た。**黙って戻らない** — 「止まった」と
        // 見分けがつかなくなる
        None if n >= cap => println!("→ {n} 命令走ったが何にも当たらない (`c <命令数>` で伸ばせる)"),
        None => {}
    }
    show_where(m);
}

/// 画面にその文字列が出るまで走らせる。**OSの起動を追う単位はこれ**
fn until(m: &mut Machine, needle: &str, cap: u64) {
    let start = m.dbg.instr;
    let mut n = 0u64;
    // 監視を切っていても命令数だけは数えたいので、目標を遠くに置いて元締めを入れる
    if !m.dbg.on {
        m.dbg.run_to(u64::MAX);
    }
    while n < cap {
        m.step();
        n += 1;
        if m.dbg.stop.is_some() {
            if let Some(s) = m.dbg.take_stop() {
                println!("{}", why(m, &s));
            }
            break;
        }
        if m.take_vram_dirty() && m.text_screen_string().contains(needle) {
            println!("{:?} を検出 ({} 命令)", needle, m.dbg.instr - start);
            break;
        }
        if m.halted && m.pending_irq.is_none() && !m.devices.pit.counters[0].running {
            println!("機械が止まった (HLT)。{:?} は出ていない", needle);
            break;
        }
    }
    if n >= cap {
        println!("→ {n} 命令走ったが {needle:?} は出ない (`until` の前に `c` で進めるか、上限を疑う)");
    }
    show_where(m);
}

/// 止まった理由を、**次に何を見ればいいかまで**含めて言う
fn why(m: &Machine, s: &Stop) -> String {
    match s {
        Stop::Break(a) => format!("→ ブレーク {a:#07x} ({} 命令目)", m.dbg.instr),
        Stop::WriteMem { addr, old, new, at } => format!(
            "→ {addr:#07x} が {old:#04x} から {new:#04x} に変わった \
             (書いたのは {:04x}:{:04x}、{} 命令目)",
            at.0, at.1, m.dbg.instr
        ),
        Stop::WriteIo { port, val, at } => format!(
            "→ ポート{port:#06x} に {val:#04x} を書いた \
             ({:04x}:{:04x}、{} 命令目)",
            at.0, at.1, m.dbg.instr
        ),
        Stop::ReadIo { port, val, at } => format!(
            "→ ポート{port:#06x} を読み {val:#04x} が返った \
             ({:04x}:{:04x}、{} 命令目)",
            at.0, at.1, m.dbg.instr
        ),
        Stop::Count(n) => format!("→ {n} 命令目"),
    }
}

fn show_where(m: &Machine) {
    let cpu = &m.cpu;
    let lin = (cpu.sregs[rustx86_core::cpu::CS] as u32) << 4 | cpu.ip as u32;
    let mut b = String::new();
    for i in 0..8 {
        b.push_str(&format!("{:02x} ", m.read8(lin.wrapping_add(i))));
    }
    println!(
        "{:>12}: {:04x}:{:04x}  {}{}",
        m.dbg.instr,
        cpu.sregs[rustx86_core::cpu::CS],
        cpu.ip,
        b,
        if m.halted { " [HLT]" } else { "" }
    );
}

fn info(m: &Machine) {
    use rustx86_core::cpu::*;
    let c = &m.cpu;
    let n = ["AX", "CX", "DX", "BX", "SP", "BP", "SI", "DI"];
    let mut s = String::new();
    for (i, name) in n.iter().enumerate() {
        s.push_str(&format!("E{name}={:08x} ", c.regs[i]));
        if i == 3 {
            s.push('\n');
        }
    }
    println!("{s}");
    println!(
        "CS={:04x} DS={:04x} ES={:04x} SS={:04x}  IP={:04x}  FLAGS={:04x} [{}]",
        c.sregs[CS],
        c.sregs[DS],
        c.sregs[ES],
        c.sregs[SS],
        c.ip,
        c.flags,
        flag_names(c),
    );
    println!(
        "命令数={}  ブレーク={:x?}  番地監視={:x?}  I/O監視 r={:x?} w={:x?}",
        m.dbg.instr, m.dbg.code, m.dbg.mem_write, m.dbg.io_read, m.dbg.io_write
    );
}

fn flag_names(c: &rustx86_core::cpu::Cpu) -> String {
    use rustx86_core::cpu::*;
    [
        (CF, "CF"), (PF, "PF"), (AF, "AF"), (ZF, "ZF"),
        (SF, "SF"), (TF, "TF"), (IF, "IF"), (DF, "DF"), (OF, "OF"),
    ]
    .iter()
    .filter(|(f, _)| c.flag(*f))
    .map(|(_, n)| *n)
    .collect::<Vec<_>>()
    .join(" ")
}

fn dump(m: &Machine, addr: u32, len: u32) {
    for row in 0..len.div_ceil(16) {
        let a = addr + row * 16;
        let mut hex = String::new();
        let mut txt = String::new();
        for i in 0..16 {
            let v = m.read8(a + i);
            hex.push_str(&format!("{v:02x} "));
            txt.push(if (0x20..0x7f).contains(&v) { v as char } else { '.' });
        }
        println!("{a:07x}  {hex} |{txt}|");
    }
}

fn trace(m: &Machine, n: usize) {
    let t = &m.dbg.trace;
    if t.is_empty() {
        println!("足跡を残していない。`record 256` を先に打つ");
        return;
    }
    for s in t.iter().skip(t.len().saturating_sub(n)) {
        let b: Vec<String> = s.bytes.iter().map(|v| format!("{v:02x}")).collect();
        println!("{:>12}: {:04x}:{:04x}  {}", s.instr, s.cs, s.ip, b.join(" "));
    }
}

/// `0x7c00` も `07c0:0000` も受ける。実機の資料は後者で書かれている
fn addr(s: &str) -> Option<u32> {
    if let Some((seg, off)) = s.split_once(':') {
        let seg = u32::from_str_radix(seg.trim_start_matches("0x"), 16).ok()?;
        let off = u32::from_str_radix(off.trim_start_matches("0x"), 16).ok()?;
        return Some(seg << 4 | off);
    }
    let t = s.trim_start_matches("0x");
    u32::from_str_radix(t, 16).ok()
}

fn help() {
    println!(
        "\
見張る
  b <番地>        実行ブレークポイント (0x7c00 でも 07c0:0000 でも可)
  w <番地>        その番地への書き込みで止める
  wi <ポート> [rw] I/Oで止める (既定は書き込みのみ)
  d               見張りを全部外す
  info            レジスタと見張りの一覧

走らせる
  c [命令数]      続行 (既定10億命令で打ち切り、走った数を言う)
  si [n]          n命令だけ進む (既定1)
  until <文字列>  画面にその文字が出るまで走らせる
  goto <命令数>   **その命令数まで巻き戻す** (最初から流し直す)

見る
  r               いまの位置
  x <番地> [長さ] メモリを16進で出す
  screen          画面
  record [n]      足跡を残し始める
  t [n]           足跡を見る

保存
  save / load     スナップショット
  q               終了"
    );
}
