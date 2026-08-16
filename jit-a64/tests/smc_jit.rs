//! JITだけが実行するページの自己書き換えが検出されること。
//!
//! core/tests/smc.rs のJIT版。あちらはインタプリタのfillが page_has_code を
//! 立てるが、**JITのbakeしか踏んでいないページ**は誰が立てるのか — 立て忘れる
//! と note_write が素通りして世代が動かず、古いブロックが走り続ける。
//! 2026-08-17にディスク課程 (virtio記述子域の上書き) がデッドハルトとして
//! これを暴いた。再発したらこのテストが赤くなる。

use rustx86_core::{cpu, Machine};

const PM_IMAGE: &[u8] = include_bytes!("../../asm/pm_hello.bin");
const CODE: u32 = 0x2000;

/// 32bitモードに入れて、コードを置いて、JITを取り付けて走らせる
fn run_pm_jit(code: &[u8]) -> Box<Machine> {
    let mut m = Box::new(Machine::new());
    m.load_boot_sector(PM_IMAGE).unwrap();
    for _ in 0..10_000 {
        if m.halted {
            break;
        }
        m.step();
    }
    assert!(m.halted, "pm_helloがHLTに着いていない");
    assert!(m.cpu.seg_is32(cpu::CS), "32bitコードに到達していない");

    for (i, b) in code.iter().enumerate() {
        m.write_phys8(CODE + i as u32, *b);
    }
    for r in [cpu::AX, cpu::CX, cpu::BX] {
        m.cpu.regs[r] = 0;
    }
    m.cpu.ip = CODE;
    m.halted = false;
    unsafe { rustx86_jit_a64::attach(&mut m) };
    // run() はチェーン実行 = チェーン入口でJITが入る
    m.run(10_000);
    assert!(m.trap.is_none(), "trapした: {:?}", m.trap);
    assert!(m.halted, "HLTに到達していない");
    m
}

#[test]
fn jitだけが実行するページの自己書き換えが次の実行に見える() {
    // core/tests/smc.rs のdword版と同じ形 — ただし**全opがJIT語彙**
    // (MovRI/AluRI/Jcc/StoreMI/Jmp) なので、ループ全体がJITで回り、
    // このページをインタプリタのfillは一度も踏まない。bakeが
    // page_has_code を立てないと、StoreMI (JIT内のストア) のnote_writeが
    // 素通りして世代が動かず、古いブロックの mov ecx,5 が走り続ける。
    // 8bitストア版にしないこと — あちらは語彙外でインタプリタに落ち、
    // fillがページを守ってしまい偽緑になる (2026-08-17に実際になった)
    let code = [
        0xB9, 0x05, 0x00, 0x00, 0x00, // 0x2000: mov ecx, 5   ← 標的
        0x81, 0xFB, 0x01, 0x00, 0x00, 0x00, // 0x2005: cmp ebx, 1
        0x74, 0x11, // 0x200B: je +0x11 → 0x201E
        0xBB, 0x01, 0x00, 0x00, 0x00, // 0x200D: mov ebx, 1
        0xC7, 0x05, 0x01, 0x20, 0x00, 0x00, 0x07, 0x00, 0x00,
        0x00, // 0x2012: mov dword [0x2001], 7 (JIT内ストア)
        0xEB, 0xE2, // 0x201C: jmp → 0x2000
        0xF4, // 0x201E: hlt
    ];
    let m = run_pm_jit(&code);
    assert_eq!(
        m.cpu.regs[cpu::CX],
        7,
        "書き換え後の mov ecx, 7 が実行されていない = JITページの世代が動いていない"
    );
}
