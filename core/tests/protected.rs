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
        let lin = (m.cpu.sregs[cpu::CS] as u32) << 4 | m.cpu.ip;
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

    // IDTRも往復する (v3)
    let mut m = boot_idt();
    run(&mut m, 10_000);
    let saved = m.save_state();
    let mut n = Machine::new();
    n.load_state(&saved).unwrap();
    assert_eq!(n.cpu.idtr_base, m.cpu.idtr_base, "IDTRが失われた");
    assert_eq!(n.cpu.idtr_limit, m.cpu.idtr_limit);
}

// ---- IDTと例外 (asm/pm_idt.asm) ----

const IDT_IMAGE: &[u8] = include_bytes!("../../asm/pm_idt.bin");

fn boot_idt() -> Machine {
    let mut m = Machine::new();
    m.load_boot_sector(IDT_IMAGE).unwrap();
    m
}

#[test]
fn ソフトウェア割り込みがゲートを通ってiretdで帰る() {
    let mut m = boot_idt();
    run(&mut m, 10_000);
    assert!(m.halted, "HLTに到達していない");
    // handler が書いた目印と、iretd で戻った直後に書いた目印の両方が要る
    assert_eq!(
        m.read32(0x504),
        0x50F7_1234,
        "int 7 のhandlerが走っていない"
    );
    assert_eq!(
        m.read32(0x508),
        0xBAC2_5AFE,
        "iretd で元の場所に戻れていない"
    );
}

#[test]
fn ud2が例外としてidt経由で配送される() {
    let mut m = boot_idt();
    run(&mut m, 10_000);
    assert!(m.halted);
    assert_eq!(m.read32(0x500), 0x0BAD_0F0B, "#UD のhandlerが走っていない");
    // フォールトから iretd で戻っていたら 0x50C に死の目印が書かれている
    assert_eq!(
        m.read32(0x50C),
        0,
        "ud2 の後ろへ落ちてきている (戻ってはいけない)"
    );
}

#[test]
fn 保護モードの割り込みはidtrを見る() {
    let mut m = boot_idt();
    run(&mut m, 10_000);
    assert!(m.cpu.pe());
    assert_ne!(m.cpu.idtr_base, 0, "LIDTが積まれていない");
    assert_eq!(m.cpu.idtr_limit, 8 * 8 - 1);
}

#[test]
fn eipは64kを越えて歩ける() {
    // これまでの全テストは偶然64K以下で走っていた。ip:u16 の名残が
    // どこかに残っていれば、ここで 0x1_0000 に折り返して発覚する
    let mut m = boot();
    run(&mut m, 10_000);
    assert!(m.cpu.pe() && m.cpu.seg_is32(cpu::CS));

    // 64Kの向こうにコードを置く: mov eax, 0xCAFE; hlt
    m.write8(0x2_0000, 0xB8);
    m.write32(0x2_0001, 0x0000_CAFE);
    m.write8(0x2_0005, 0xF4);
    m.halted = false;
    m.cpu.set_ip(0x2_0000);
    for _ in 0..10 {
        if m.halted {
            break;
        }
        m.step();
    }
    assert!(m.halted);
    assert_eq!(
        m.cpu.regs[cpu::AX],
        0xCAFE,
        "64K越えのコードが実行されていない"
    );
    assert_eq!(m.cpu.ip, 0x2_0006, "EIPが64Kで折り返している");
}

#[test]
fn リアルモードのipは64kで折り返す() {
    // 8086の挙動。EIPを32bitにしても、16bitコードでは64Kの環で回り続ける
    let mut m = Machine::new();
    let mut sector = vec![0u8; 512];
    sector[510] = 0x55;
    sector[511] = 0xAA;
    m.load_boot_sector(&sector).unwrap();
    m.write8(0xFFFF, 0x90); // CS=0 の末尾に NOP
    m.cpu.set_cs_ip(0, 0xFFFF);
    m.step();
    assert_eq!(m.cpu.ip, 0, "リアルモードで64Kを越えてしまった");
}

// ---- 特権リング (asm/pm_ring.asm) ----

const RING_IMAGE: &[u8] = include_bytes!("../../asm/pm_ring.bin");

fn boot_ring() -> Machine {
    let mut m = Machine::new();
    m.load_boot_sector(RING_IMAGE).unwrap();
    m
}

#[test]
fn リング3へ降りて0へ落ちて3へ帰る() {
    let mut m = boot_ring();
    run(&mut m, 10_000);
    assert!(m.halted, "HLTに到達していない");
    assert_eq!(m.read32(0x500), 0x0000_3333, "リング3に降りられていない");
    assert_eq!(
        m.read32(0x504),
        0x0000_C0DE,
        "int 0x30 がリング0に届いていない"
    );
    assert_eq!(
        m.read32(0x508),
        0x00BA_C703,
        "iretd でリング3へ帰れていない"
    );
}

#[test]
fn リング3ではcplが3になる() {
    let mut m = boot_ring();
    // 0x500 に目印が書かれた瞬間 = リング3で走っている
    m.dbg.watch_mem(0x500);
    for _ in 0..10_000 {
        if m.dbg.stop.is_some() || m.halted {
            break;
        }
        m.step();
    }
    assert!(m.dbg.take_stop().is_some(), "リング3に到達していない");
    assert_eq!(m.cpu.cpl(), 3, "CPLが3でない");
    assert_eq!(m.cpu.sregs[cpu::CS], 0x1B, "CSのRPLが3でない");
}

#[test]
fn リング0の受け手ではスタックがtssのものに差し替わる() {
    let mut m = boot_ring();
    // 0x504 の目印 = svc_handler の中 (リング0)
    m.dbg.watch_mem(0x504);
    for _ in 0..10_000 {
        if m.dbg.stop.is_some() || m.halted {
            break;
        }
        m.step();
    }
    assert!(
        m.dbg.take_stop().is_some(),
        "リング0の受け手に到達していない"
    );
    assert_eq!(m.cpu.cpl(), 0);
    assert_eq!(m.cpu.sregs[cpu::SS] & !3, 0x10, "SSがTSSのSS0でない");
    // ESP0=0x6000 から [EIP,CS,EFLAGS,ESP,SS] の5つ (20バイト) が積まれた状態
    assert_eq!(
        m.cpu.regs[cpu::SP] as u16,
        0x6000 - 20,
        "ESPがTSSのESP0基準でない"
    );
}
