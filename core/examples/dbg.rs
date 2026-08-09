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
        // 512バイトならブートセクタ単体 (asm/*.bin)、それ以外はディスクイメージ
        if data.len() == 512 {
            m.load_boot_sector(&data).expect("boot sector");
        } else {
            m.boot_from_disk(data.clone()).expect("boot");
        }
        disk = Some(data);
    }

    println!("rustx86 debugger.  `help` for commands, `q` to quit");
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
                    println!("break on execute at {a:#07x}");
                }
                None => println!("usage: b 0x7c00  |  b 07c0:0000"),
            },
            "w" | "watch" => match arg1.and_then(addr) {
                Some(a) => {
                    m.dbg.watch_mem(a);
                    println!("break on write to {a:#07x}");
                }
                None => println!("usage: w 0x450"),
            },
            "wi" => match arg1.and_then(addr).map(|a| a as u16) {
                Some(p) => {
                    let rw = rest.get(1).copied().unwrap_or("w");
                    m.dbg.watch_io(p, rw.contains('r'), rw.contains('w'));
                    println!("break on I/O port {p:#06x} ({rw})");
                }
                None => println!("usage: wi 0x3d5 [rw]"),
            },
            "d" | "delete" => {
                m.dbg.clear();
                println!("all watchpoints cleared");
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
                let needle = line
                    .split_once(char::is_whitespace)
                    .map(|x| x.1)
                    .unwrap_or("")
                    .trim();
                if needle.is_empty() {
                    println!("usage: until FreeDOS kernel");
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
                        if d.len() == 512 {
                            m.load_boot_sector(d).unwrap();
                        } else {
                            m.boot_from_disk(d.clone()).unwrap();
                        }
                    }
                    m.dbg.record_trace(cap);
                    m.dbg.run_to(n);
                    print!("replaying to instruction {n}… ");
                    let _ = std::io::stdout().flush();
                    run(&mut m, n + 1);
                    m.dbg.code = code;
                    m.dbg.mem_write = mem;
                    m.dbg.io_write = iow;
                    m.dbg.io_read = ior;
                }
                None => println!("usage: goto 36000000"),
            },

            // --- 見る ---
            "r" | "reg" => show_where(&m),
            "x" => match arg1.and_then(addr) {
                Some(a) => {
                    let n = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(64);
                    dump(&m, a, n);
                }
                None => println!("usage: x 0x400 [len]   (0x400 = BIOS Data Area)"),
            },
            "screen" => println!("{}", m.text_screen_string()),
            "t" | "trace" => {
                let n = arg1.and_then(|s| s.parse().ok()).unwrap_or(16);
                trace(&m, n);
            }
            "record" => {
                let n = arg1.and_then(|s| s.parse().ok()).unwrap_or(256);
                m.dbg.record_trace(n);
                println!("recording the last {n} instructions");
            }

            // --- 保存 ---
            "save" => {
                snap = Some(m.save_state());
                println!("saved at instruction {}", m.dbg.instr);
            }
            "load" => match &snap {
                Some(s) => match m.load_state(s) {
                    Ok(()) => show_where(&m),
                    Err(e) => println!("cannot restore: {e}"),
                },
                None => println!("nothing saved yet"),
            },

            _ => println!("unknown command `{cmd}`.  try `help`"),
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
            println!("the machine stopped for good (HLT)");
            break;
        }
    }
    match m.dbg.take_stop() {
        Some(s) => println!("{}", why(m, &s)),
        // 何にも当たらずに上限へ来た。**黙って戻らない** — 「止まった」と
        // 見分けがつかなくなる
        None if n >= cap => {
            println!("-> ran {n} instructions, nothing hit (`c <count>` to run longer)")
        }
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
            println!(
                "found {:?} after {} instructions",
                needle,
                m.dbg.instr - start
            );
            break;
        }
        if m.halted && m.pending_irq.is_none() && !m.devices.pit.counters[0].running {
            println!("the machine stopped for good (HLT); {needle:?} never appeared");
            break;
        }
    }
    if n >= cap {
        println!("-> ran {n} instructions, {needle:?} never appeared");
    }
    show_where(m);
}

/// 止まった理由。**表示はブラウザ側と同じ文面にする** —
/// 同じ道具が窓によって違う言い方をすると、検索も比較もできなくなる
fn why(m: &Machine, s: &Stop) -> String {
    let n = m.dbg.instr;
    match s {
        Stop::Break(a) => format!("-> breakpoint at {a:#07x} (instr {n})"),
        Stop::WriteMem { addr, old, new, at } => format!(
            "-> {addr:#07x} changed {old:#04x} -> {new:#04x} by {:04x}:{:04x} (instr {n})",
            at.0, at.1
        ),
        Stop::WriteIo { port, val, at } => format!(
            "-> wrote {val:#04x} to port {port:#06x} by {:04x}:{:04x} (instr {n})",
            at.0, at.1
        ),
        Stop::ReadIo { port, val, at } => format!(
            "-> read {val:#04x} from port {port:#06x} by {:04x}:{:04x} (instr {n})",
            at.0, at.1
        ),
        Stop::Count(n) => format!("-> reached instruction {n}"),
    }
}

fn show_where(m: &Machine) {
    let cpu = &m.cpu;
    let lin = cpu.lin(rustx86_core::cpu::CS, cpu.ip);
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
    // モードと、その根拠。保護モードで死ぬときの手掛かりは大抵ここにある
    if c.pe() {
        println!(
            "mode=protected  CR0={:08x}  GDTR={:08x}+{:04x}  IDTR={:08x}+{:04x}",
            c.cr0, c.gdtr_base, c.gdtr_limit, c.idtr_base, c.idtr_limit
        );
        for (name, i) in [("CS", CS), ("DS", DS), ("SS", SS)] {
            let h = &c.hidden[i];
            println!(
                "  {name}={:04x} -> base={:08x} limit={:08x} {} access={:02x}",
                c.sregs[i],
                h.base,
                h.limit,
                if h.big { "32bit" } else { "16bit" },
                h.access,
            );
        }
    } else {
        println!("mode=real  CR0={:08x}", c.cr0);
    }
    println!(
        "instrs={}  execute={:x?}  write={:x?}  I/O read={:x?} write={:x?}",
        m.dbg.instr, m.dbg.code, m.dbg.mem_write, m.dbg.io_read, m.dbg.io_write
    );
}

fn flag_names(c: &rustx86_core::cpu::Cpu) -> String {
    use rustx86_core::cpu::*;
    [
        (CF, "CF"),
        (PF, "PF"),
        (AF, "AF"),
        (ZF, "ZF"),
        (SF, "SF"),
        (TF, "TF"),
        (IF, "IF"),
        (DF, "DF"),
        (OF, "OF"),
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
            txt.push(if (0x20..0x7f).contains(&v) {
                v as char
            } else {
                '.'
            });
        }
        println!("{a:07x}  {hex} |{txt}|");
    }
}

fn trace(m: &Machine, n: usize) {
    let t = &m.dbg.trace;
    if t.is_empty() {
        println!("nothing recorded.  run `record 256` first");
        return;
    }
    for s in t.iter().skip(t.len().saturating_sub(n)) {
        let b: Vec<String> = s.bytes.iter().map(|v| format!("{v:02x}")).collect();
        println!(
            "{:>12}: {:04x}:{:04x}  {}",
            s.instr,
            s.cs,
            s.ip,
            b.join(" ")
        );
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
watch — 止めて、どの命令がやったかまで言う
  b <addr>         その番地を実行する直前で止める (0x7c00 でも 07c0:0000 でも)
  w <addr>         その番地に書き込んだら止める
  wi <port> [rw]   I/Oで止める (既定は書き込みのみ)
  d                見張りを全部外す
  info             レジスタと見張りの一覧

run
  c [count]        続行 (既定10億命令で打ち切り、走った数を必ず言う)
  si [n]           n命令だけ進む (既定1)
  until <text>     画面にその文字が出るまで走らせる
  goto <count>     その命令数まで巻き戻す (最初から流し直す)

look
  r                いまの位置
  x <addr> [len]   メモリを16進で (既定 0x400 = BIOSデータエリア)
  screen           ゲストの画面
  record [n]       実行した命令を残し始める
  t [n]            残したものを見る

save
  save / load      スナップショット
  q                終了"
    );
}
