//! 自己書き換え (SMC) がデコード済みキャッシュに正しく届くこと。
//!
//! ADR-0008 (2026-08-11追記) が記録した既知の穴: note_write を呼ぶのは
//! write_phys8・REP文字列・virtio-blk だけで、**ゲストの通常ストア
//! (write8/write_wide/fast_write*) と fast RMW の直書きは検出網の外**だった。
//! Linuxが無事だったのは text_poke が memcpy = REP MOVS 経由で網に入る偶然
//! (と推測) による。ここでは網の外だった3経路を1つずつ踏む:
//!
//!   1. mov byte  [mem], imm  → fast_write8 / write8
//!   2. add byte  [mem], imm  → fast_rmw8_addr の直書き (dcache/exec.rs)
//!   3. mov dword [mem], imm  → fast_write32 / write_wide
//!
//! 形は3つとも同じ: 命令を一度実行して dcache に載せる → その命令自身を
//! ゲストのストアで書き換える → 同じ番地へ戻る → **新しい命令が実行される**
//! ことを確かめる。古い写しが実行されたら、それが穴である。

use rustx86_core::{cpu, Machine};

/// pm_hello で32bitプロテクトモードまで運ぶ (CS=0x08 flat 32bit、DS=0x10 flat)。
/// dcache は32bitコード (CS.D=1) でだけ働くので、この足場が要る
const PM_IMAGE: &[u8] = include_bytes!("../../asm/pm_hello.bin");

const CODE: u32 = 0x2000;

/// 32bitモードに入った機械に命令列を置いて走らせる
fn run_pm(code: &[u8]) -> Machine {
    let mut m = Machine::new();
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
    m.run(10_000);
    assert!(m.trap.is_none(), "trapした: {:?}", m.trap);
    assert!(m.halted, "HLTに到達していない");
    m
}

/// 1周目: inc eax を実行して控えに載せ、自分を inc ecx (0x41) に書き換える。
/// 2周目: 書き換え後の命令が実行されるはず。patch はテストごとに差し替える
fn smc_loop_8bit(patch: [u8; 7]) -> Machine {
    let mut code = vec![
        0x40, // 0x2000: inc eax            ← 標的 (1周目に控えへ載る)
        0x81, 0xFB, 0x01, 0x00, 0x00, 0x00, // 0x2001: cmp ebx, 1
        0x74, 0x0E, // 0x2007: je +0x0E → 0x2017 (2周目はここで抜ける)
        0xBB, 0x01, 0x00, 0x00, 0x00, // 0x2009: mov ebx, 1 (2周目の印)
    ];
    code.extend_from_slice(&patch); // 0x200E: 標的を 0x41 に書き換える7バイト
    code.extend_from_slice(&[
        0xEB, 0xE9, // 0x2015: jmp → 0x2000 (標的へ戻る)
        0xF4, // 0x2017: hlt
    ]);
    run_pm(&code)
}

#[test]
fn movストアの自己書き換えが次の実行に見える() {
    // mov byte [0x2000], 0x41 — fast_write8 / write8 の経路
    let m = smc_loop_8bit([0xC6, 0x05, 0x00, 0x20, 0x00, 0x00, 0x41]);
    assert_eq!(m.cpu.regs[cpu::AX], 1, "inc eax は1周目の1回だけのはず");
    assert_eq!(
        m.cpu.regs[cpu::CX],
        1,
        "書き換え後の inc ecx が実行されていない = 古い写しが生きている"
    );
}

#[test]
fn rmwストアの自己書き換えが次の実行に見える() {
    // add byte [0x2000], 1 (0x40 + 1 = 0x41) — fast_rmw8_addr 直書きの経路
    let m = smc_loop_8bit([0x80, 0x05, 0x00, 0x20, 0x00, 0x00, 0x01]);
    assert_eq!(m.cpu.regs[cpu::AX], 1, "inc eax は1周目の1回だけのはず");
    assert_eq!(
        m.cpu.regs[cpu::CX],
        1,
        "書き換え後の inc ecx が実行されていない = 古い写しが生きている"
    );
}

#[test]
fn dwordストアの自己書き換えが次の実行に見える() {
    // 標的は mov ecx, 5。即値フィールド4バイトを mov dword [0x2001], 7 で
    // 書き換える — fast_write32 / write_wide の経路
    let code = [
        0xB9, 0x05, 0x00, 0x00, 0x00, // 0x2000: mov ecx, 5   ← 標的
        0x81, 0xFB, 0x01, 0x00, 0x00, 0x00, // 0x2005: cmp ebx, 1
        0x74, 0x11, // 0x200B: je +0x11 → 0x201E
        0xBB, 0x01, 0x00, 0x00, 0x00, // 0x200D: mov ebx, 1
        0xC7, 0x05, 0x01, 0x20, 0x00, 0x00, 0x07, 0x00, 0x00,
        0x00, // 0x2012: mov dword [0x2001], 7
        0xEB, 0xE2, // 0x201C: jmp → 0x2000
        0xF4, // 0x201E: hlt
    ];
    let m = run_pm(&code);
    assert_eq!(
        m.cpu.regs[cpu::CX],
        7,
        "書き換え後の mov ecx, 7 が実行されていない = 古い写しが生きている"
    );
}
