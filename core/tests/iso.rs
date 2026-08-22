//! ISO 起動 (6c、El Torito)。isolinux + 自前の Linux を焼いた ISO から、BIOS の CD
//! (INT 13h ドライブ 0xE0、EDD 読み) 経由でカーネルが上がる。像は make-test-iso.sh
use rustx86_core::{Machine, MachineProfile};

const ISO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../images/test-linux.iso");

#[test]
fn isolinux_boots_linux_from_iso() {
    let path = std::env::var("RUSTX86_ISO").unwrap_or_else(|_| ISO.to_string());
    let Ok(iso) = std::fs::read(&path) else {
        eprintln!("skip: {path} が無い (tools/images/sh/make-test-iso.sh)");
        return;
    };
    let mut m = Machine::with_profile(MachineProfile::pc_floppy(128));
    m.boot_from_iso(iso).expect("El Torito");
    // ISO_TRACE=1: まず INT 13h AH=42 を直接呼んで読みの正しさを検算し、
    // 最初に HLT するまでの直前 60 命令 (重複畳み) を出す
    if std::env::var("ISO_TRACE").is_ok() {
        let iso0 = std::fs::read(&path).unwrap();
        // DAP を 0:0500 に組む: 15 セクタ、0880:0000 へ、LBA 35
        let dap = 0x500u32;
        m.write8(dap, 0x10);
        m.write8(dap + 1, 0);
        m.write16(dap + 2, 15);
        m.write16(dap + 4, 0);
        m.write16(dap + 6, 0x0880);
        m.write32(dap + 8, 35);
        m.write32(dap + 12, 0);
        m.cpu.regs[rustx86_core::cpu::AX] = 0x4200;
        m.cpu.regs[rustx86_core::cpu::DX] = 0x00E0;
        m.cpu.regs[rustx86_core::cpu::SI] = 0x500;
        m.cpu.sregs[3] = 0; // DS
        m.bios_interrupt(0x13);
        let bad = (0..15 * 2048)
            .filter(|&i| m.read8(0x8800 + i as u32) != iso0[35 * 2048 + i])
            .count();
        eprintln!(
            "直接 AH=42 (LBA35×15 → 0880:0000): 違い {bad} / AH={:#x} CF={}",
            m.cpu.regs[0] >> 8,
            m.cpu.flag(rustx86_core::cpu::CF)
        );
        let mut ring: std::collections::VecDeque<(u16, u32, [u8; 8])> =
            std::collections::VecDeque::new();
        for _ in 0..3_000_000_000u64 {
            let lin = m.cpu.lin(rustx86_core::cpu::CS, m.cpu.ip);
            let mut b = [0u8; 8];
            for (i, x) in b.iter_mut().enumerate() {
                *x = m.read8(lin + i as u32);
            }
            let k = (m.cpu.sregs[1], m.cpu.ip);
            if ring.back().map(|e| (e.0, e.1)) != Some(k) {
                ring.push_back((k.0, k.1, b));
                if ring.len() > 60 {
                    ring.pop_front();
                }
            }
            m.step();
            if m.halted {
                break;
            }
        }
        // 読んだ中身の検算: 最後の AH=42 は LBA 21 → 1000:0000、最初は LBA 35 ×15 → 0880:0000
        let iso = std::fs::read(&path).unwrap();
        let same = |lin: usize, lba: usize, n: usize| -> usize {
            (0..n)
                .filter(|&i| m.read8((lin + i) as u32) != iso[lba * 2048 + i])
                .count()
        };
        eprintln!(
            "検算: 1000:0000 vs LBA21 違い={} / 0880:0000 vs LBA35(15 sect) 違い={}",
            same(0x10000, 21, 2048),
            same(0x8800, 35, 15 * 2048)
        );
        // isolinux.bin (ISO の中の実体) と 0x7C00 からのメモリを 2KB ごとに比べる
        if let Ok(lba) = std::env::var("ISOLINUX_LBA") {
            let lba: usize = lba.parse().unwrap();
            let bin = &iso[lba * 2048..lba * 2048 + 43008];
            for (at, blk) in [
                (0x8400usize, 1usize),
                (0x8800, 1),
                (0x8800, 2),
                (0x8c00, 2),
                (0x9000, 2),
                (0x9000, 3),
            ] {
                let d = (0..2048)
                    .filter(|&i| m.read8((at + i) as u32) != bin[blk * 2048 + i])
                    .count();
                eprintln!("  mem@{at:#x} vs bin block {blk}: 違い {d}");
            }
            for k in 0..21 {
                let d = (0..2048)
                    .filter(|&i| m.read8((0x7C00 + k * 2048 + i) as u32) != bin[k * 2048 + i])
                    .count();
                eprintln!(
                    "  isolinux.bin 2KB #{k} @{:#x}: 違い {d}",
                    0x7C00 + k * 2048
                );
            }
        }
        let ivt = |m: &Machine, n: u32| (m.read16(n * 4 + 2), m.read16(n * 4));
        let t0 = (m.read32(0x46C), m.read32(0x8EBC));
        m.run(50_000_000);
        let t1 = (m.read32(0x46C), m.read32(0x8EBC));
        eprintln!(
            "IVT[08]={:04x?} IVT[1C]={:04x?} | BDA 46C {} → {} | [0x8EBC] {} → {}",
            ivt(&m, 8),
            ivt(&m, 0x1C),
            t0.0,
            t1.0,
            t0.1,
            t1.1
        );
        // 1MB へ写された PM 部分の検算: メモリ 0x101B91 の並びを ISO の中で探し、前後 0x400 を比べる
        let pat: Vec<u8> = (0..16).map(|i| m.read8(0x101B91 + i)).collect();
        if let Some(pos) = iso0.windows(16).position(|w| w == pat.as_slice()) {
            let base_mem = 0x101B91u32 - 0x400;
            let base_iso = pos - 0x400;
            let diffs: Vec<String> = (0..0x800usize)
                .filter(|&i| m.read8(base_mem + i as u32) != iso0[base_iso + i])
                .take(12)
                .map(|i| {
                    format!(
                        "{:#x}:{:02x}/{:02x}",
                        base_mem + i as u32,
                        m.read8(base_mem + i as u32),
                        iso0[base_iso + i]
                    )
                })
                .collect();
            let n = (0..0x800usize)
                .filter(|&i| m.read8(base_mem + i as u32) != iso0[base_iso + i])
                .count();
            eprintln!(
                "PM 部分 (ISO offset {pos:#x} ≈ LBA {}): 0x800 バイト中 {n} 違い: {}",
                pos / 2048,
                diffs.join(" ")
            );
        } else {
            eprintln!("PM 部分の並びが ISO に無い (写し先が壊れている)");
        }
        eprintln!(
            "a20={} [0x4d6c]={:#x} [0x104d6c]={:#x} halted={} cs:ip={:04x}:{:x}",
            m.cpu.a20,
            m.read32(0x4d6c),
            m.read32(0x104d6c),
            m.halted,
            m.cpu.sregs[1],
            m.cpu.ip
        );
        eprintln!("最初の HLT まで (tsc={}) の直前:", m.cpu.tsc);
        for (cs, ip, b) in ring.iter() {
            let t: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
            eprintln!("  {cs:04x}:{ip:06x} {}", t.join(" "));
        }
    }
    let mut serial = String::new();
    let mut reached = false;
    for _ in 0..400u32 {
        m.run(5_000_000);
        serial.push_str(&m.devices.uart.tx_string());
        m.devices.uart.tx.clear();
        if serial.contains("busybox shell") {
            reached = true;
            break;
        }
        if m.trap.is_some() {
            break;
        }
    }
    if !reached {
        eprintln!(
            "int_counts 10h={} 13h={} 15h={} 16h={} 1Ah={} | cs:ip={:04x}:{:x} halted={} pe={}",
            m.int_counts[0x10],
            m.int_counts[0x13],
            m.int_counts[0x15],
            m.int_counts[0x16],
            m.int_counts[0x1A],
            m.cpu.sregs[1],
            m.cpu.ip,
            m.halted,
            m.cpu.pe()
        );
        eprintln!(
            "IF={} pit0 running={} pic0 imr={:#x} idt={:#x}/{:#x} pending_irq={:?} tsc={}",
            m.cpu.flag(rustx86_core::cpu::IF),
            m.devices.pit.counters[0].running,
            m.devices.pic[0].imr,
            m.cpu.idtr_base,
            m.cpu.idtr_limit,
            m.pending_irq,
            m.cpu.tsc
        );
        let mut seen = std::collections::BTreeMap::new();
        for _ in 0..20_000 {
            *seen.entry((m.cpu.sregs[1], m.cpu.ip)).or_insert(0u32) += 1;
            m.step();
        }
        let mut v: Vec<_> = seen.into_iter().collect();
        v.sort_by_key(|e| std::cmp::Reverse(e.1));
        for ((cs, ip), n) in v.iter().take(10) {
            let lin = m.cpu.lin(rustx86_core::cpu::CS, *ip);
            let b: Vec<String> = (0..8)
                .map(|i| format!("{:02x}", m.read8(lin + i)))
                .collect();
            eprintln!("  {cs:04x}:{ip:08x} x{n} {}", b.join(" "));
        }
    }
    assert!(
        reached,
        "ISO からシェルに届かない。trap={:?}\n画面:\n{}\nシリアル末尾:\n{}",
        m.trap.as_ref().map(|t| t.reason.clone()),
        m.text_screen_string(),
        &serial[serial.len().saturating_sub(1500)..]
    );
}

/// dcache 経路 (A) と従来経路 (B、dbg.on) を ISO 起動で並走させ、RAM/レジスタが最初にズレた所を出す
#[test]
#[ignore]
fn iso_lockstep() {
    let path = std::env::var("RUSTX86_ISO").unwrap_or_else(|_| ISO.to_string());
    let iso = std::fs::read(&path).expect("iso");
    let boot = |dbg: bool| {
        let mut m = Machine::with_profile(MachineProfile::pc_floppy(128));
        m.boot_from_iso(iso.clone()).expect("El Torito");
        m.dbg.on = dbg;
        m
    };
    let (mut a, mut b) = (boot(false), boot(true));
    let mut prev: (u16, u32);
    for step in 0..300_000_000u64 {
        prev = (a.cpu.sregs[1], a.cpu.ip);
        a.step();
        b.step();
        if (61_000..61_200).contains(&step) && a.read8(0x100CD4) != b.read8(0x100CD4) {
            let lin = a.cpu.lin(rustx86_core::cpu::CS, prev.1);
            let bytes: Vec<String> = (0..12)
                .map(|i| format!("{:02x}", b.read8(lin + i)))
                .collect();
            eprintln!("0x100CD4 がズレた: {step} 歩目、直前の命令 {:04x}:{:x} = {} | A={:02x} B={:02x}\n  A regs={:08x?} esp={:x} sregs={:x?}\n  B regs={:08x?}",
                prev.0, prev.1, bytes.join(" "), a.read8(0x100CD4), b.read8(0x100CD4), a.cpu.regs, a.cpu.regs[4], a.cpu.sregs, b.cpu.regs);
            break;
        }
        if step % 5_000 == 0 || (step >= 60_000 && step % 50 == 0) {
            let (ma, mb): (&[u8], &[u8]) = (&a.mem[..0x200000], &b.mem[..0x200000]);
            if ma != mb {
                let diffs: Vec<String> = (0..ma.len())
                    .filter(|&i| ma[i] != mb[i])
                    .take(10)
                    .map(|i| format!("{i:#x}:{:02x}/{:02x}", ma[i], mb[i]))
                    .collect();
                let n = (0..ma.len()).filter(|&i| ma[i] != mb[i]).count();
                eprintln!(
                    "RAM がズレた: {step} 歩目までに {n} バイト: {}",
                    diffs.join(" ")
                );
                break;
            }
        }
        let same = a.cpu.regs == b.cpu.regs
            && a.cpu.ip == b.cpu.ip
            && a.cpu.sregs == b.cpu.sregs
            && a.cpu.eflags() == b.cpu.eflags();
        if !same {
            let lin = a.cpu.lin(rustx86_core::cpu::CS, prev.1);
            let bytes: Vec<String> = (0..10)
                .map(|i| format!("{:02x}", b.read8(lin + i)))
                .collect();
            eprintln!("レジスタがズレた: {step} 歩目。直前の命令 {:04x}:{:x} = {} (B の写し)\n  A regs={:08x?} ip={:x} fl={:#x}\n  B regs={:08x?} ip={:x} fl={:#x}",
                prev.0, prev.1, bytes.join(" "), a.cpu.regs, a.cpu.ip, a.cpu.eflags(), b.cpu.regs, b.cpu.ip, b.cpu.eflags());
            break;
        }
        if a.halted && b.halted && a.cpu.tsc > 3_000_000 {
            eprintln!("両方 HLT (ズレなし、{step} 歩)");
            break;
        }
    }
}
