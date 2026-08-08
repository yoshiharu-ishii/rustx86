//! プロテクトモードへの遷移 (Tier 3a)。
//!
//! `asm/pm_hello.asm` は LGDT → CR0.PE=1 → far jump → 32bit命令 → HLT を
//! 通る最小のコードで、**これが通るまでに要るものが Tier 3a の実装リスト**。
//! リアルモードの hello.bin と同じ「最初の一歩」の位置づけである。

use rustx86_core::{cpu, Machine};

const IMAGE: &[u8] = include_bytes!("../../asm/pm_hello.bin");

fn boot() -> Machine {
    let mut m = Machine::new();
    m.load_boot_sector(IMAGE).unwrap();
    m
}

/// 走らせる。HLTで止まるか、上限に達したら打ち切り
fn run(m: &mut Machine, cap: u64) {
    for _ in 0..cap {
        if m.halted {
            break;
        }
        m.step();
    }
}

#[test]
fn プロテクトモードに入って32bit命令が動く() {
    let mut m = boot();
    run(&mut m, 10_000);

    assert!(m.halted, "HLTに到達していない");
    // 32bitコードが実際に走った証拠。目印は EAX とメモリの両方に残る
    assert_eq!(
        m.cpu.regs[cpu::AX],
        0x32B1_7600,
        "EAXの目印が無い = 32bitコードに到達していない"
    );
    assert_eq!(
        m.read32(0x500),
        0x32B1_7600,
        "32bitアドレッシングの書き込みが落ちている"
    );
}

#[test]
fn far_jumpの前はまだ16bitで動いている() {
    // PE=1 を立てた**あと**、far jump の**前**は、モードとしては保護モードだが
    // 実行はまだ16bitセグメントのまま。これが実機の2段構え。
    // far jump 命令 (EA) の直前で止めて確かめる
    let mut m = boot();
    // EA (far jump) の実行前 = CS:IP がまだリアルモードの形
    for _ in 0..10_000 {
        if m.halted {
            panic!("far jumpに着く前にHLTした");
        }
        let lin = (m.cpu.sregs[cpu::CS] as u32) << 4 | m.cpu.ip as u32;
        if m.read8(lin) == 0xEA {
            // ここが境界。PEはもう立っている
            assert_eq!(m.cpu.cr0 & 1, 1, "far jump時点でPEが立っていない");
            return;
        }
        m.step();
    }
    panic!("far jump (EA) を通らなかった");
}

#[test]
fn セレクタの裏に記述子が積まれる() {
    let mut m = boot();
    run(&mut m, 10_000);
    assert!(m.halted);

    // far jump で CS に 0x08 が、その裏に隠しレジスタ (base=0, 32bit) が積まれる
    assert_eq!(m.cpu.sregs[cpu::CS], 0x08, "CSがセレクタになっていない");
    assert_eq!(m.cpu.seg_base(cpu::CS), 0, "CSのbaseが0でない");
    assert!(m.cpu.seg_is32(cpu::CS), "CSがDビット=32bitになっていない");

    // MOV DS,AX でデータ側にも積まれる
    assert_eq!(m.cpu.sregs[cpu::DS], 0x10);
    assert_eq!(m.cpu.seg_base(cpu::DS), 0);
}

#[test]
fn 保護モードの状態はスナップショットで往復する() {
    // 隠しレジスタを落とすと、復元した瞬間に全アドレスが嘘になる —
    // セレクタ 0x08 から base=0 は再構成できない (GDTは書き換わっているかもしれない)。
    // だから CR0・GDTR・隠しレジスタは機械の状態そのものとして保存される
    let mut m = boot();
    run(&mut m, 10_000);
    assert!(m.halted && m.cpu.pe());

    let saved = m.save_state();
    let mut n = Machine::new();
    n.load_state(&saved).unwrap();

    assert!(n.cpu.pe(), "PEが失われた");
    assert_eq!(n.cpu.sregs[cpu::CS], 0x08);
    assert_eq!(n.cpu.seg_base(cpu::CS), 0, "隠しレジスタのbaseが失われた");
    assert!(n.cpu.seg_is32(cpu::CS), "Dビットが失われた");
    assert_eq!(n.cpu.gdtr_base, m.cpu.gdtr_base);
    assert_eq!(n.read32(0x500), 0x32B1_7600);
}
