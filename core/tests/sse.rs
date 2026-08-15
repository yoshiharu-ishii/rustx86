//! SSE のシャッフル (`shufps` / `shufpd`)。
//!
//! **gcc の cc1 がここで #UD になっていた** (2026-08-15)。`shufps` は実装済みで、
//! 66プレフィクス付きの兄弟 `shufpd` だけが抜けていた — 語彙の歯抜けは
//! 「使うゲストが来て初めて見える」ことの実例なので、両方を釘で打っておく。
//!
//! MMX/SSE の命令選択子は**生の66プレフィクスの有無**で決まり、既定オペランド幅
//! とは無関係なので、コードはリアルモードに置いてよい (mmx.rs と同じ流儀)。

use rustx86_core::{Machine, MachineProfile};

const BASE: u32 = 0x0600;
const DATA: u32 = 0x0700;

fn mach() -> Machine {
    Machine::with_profile(MachineProfile::pc_32bit(4))
}

fn run(m: &mut Machine, code: &[u8]) {
    for (i, b) in code.iter().enumerate() {
        m.write8(BASE + i as u32, *b);
    }
    m.write8(BASE + code.len() as u32, 0xF4); // hlt
    m.cpu.ip = BASE;
    m.halted = false;
    m.run(code.len() as u64 + 16);
    assert!(m.trap.is_none(), "trapした: {:?}", m.trap);
    assert!(m.halted, "hltまで到達していない");
}

fn write128(m: &mut Machine, a: u32, v: u128) {
    for (i, b) in v.to_le_bytes().iter().enumerate() {
        m.write8(a + i as u32, *b);
    }
}

/// **`shufpd xmm0, xmm1, imm8`** — 64bitレーンを2つ選ぶ。
/// 低位は自分 (dest) の2択、高位は相手 (src) の2択。
#[test]
fn shufpd_picks_one_qword_from_each_operand() {
    // imm8 の下2bitだけが効く。4通り全部を確かめる
    for (imm, want) in [
        (0b00u8, 0x2222_2222_2222_2222_1111_1111_1111_1111u128),
        (0b01, 0x2222_2222_2222_2222_AAAA_AAAA_AAAA_AAAAu128),
        (0b10, 0xBBBB_BBBB_BBBB_BBBB_1111_1111_1111_1111u128),
        (0b11, 0xBBBB_BBBB_BBBB_BBBB_AAAA_AAAA_AAAA_AAAAu128),
    ] {
        let mut m = mach();
        // dest = AAAA…(高) : 1111…(低) / src = BBBB…(高) : 2222…(低)
        write128(&mut m, DATA, 0xAAAA_AAAA_AAAA_AAAA_1111_1111_1111_1111);
        write128(&mut m, DATA + 16, 0xBBBB_BBBB_BBBB_BBBB_2222_2222_2222_2222);
        #[rustfmt::skip]
        let code: Vec<u8> = vec![
            0x66, 0x0F, 0x10, 0x06, (DATA & 0xFF) as u8, (DATA >> 8) as u8,        // movupd xmm0,[DATA]
            0x66, 0x0F, 0x10, 0x0E, ((DATA + 16) & 0xFF) as u8, ((DATA + 16) >> 8) as u8, // movupd xmm1,[DATA+16]
            0x66, 0x0F, 0xC6, 0xC1, imm,                                            // shufpd xmm0,xmm1,imm
            0x66, 0x0F, 0x11, 0x06, ((DATA + 32) & 0xFF) as u8, ((DATA + 32) >> 8) as u8, // movupd [DATA+32],xmm0
        ];
        run(&mut m, &code);
        let got = (0..16).fold(0u128, |acc, i| {
            acc | ((m.read8(DATA + 32 + i) as u128) << (i * 8))
        });
        assert_eq!(got, want, "shufpd imm={imm:#04b} の結果が違う");
    }
}

/// **`shufps`** は32bitレーンを4つ選ぶ (低2つは自分、高2つは相手)。
/// shufpd と取り違えていないことの対比として置く
#[test]
fn shufps_picks_four_dwords() {
    let mut m = mach();
    write128(&mut m, DATA, 0x4444_4444_3333_3333_2222_2222_1111_1111);
    write128(&mut m, DATA + 16, 0x8888_8888_7777_7777_6666_6666_5555_5555);
    // imm=0b11_10_01_00 = 恒等 (低位2つはdestの0,1 / 高位2つはsrcの2,3)
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x0F, 0x10, 0x06, (DATA & 0xFF) as u8, (DATA >> 8) as u8,                     // movups xmm0,[DATA]
        0x0F, 0x10, 0x0E, ((DATA + 16) & 0xFF) as u8, ((DATA + 16) >> 8) as u8,       // movups xmm1,[DATA+16]
        0x0F, 0xC6, 0xC1, 0b11_10_01_00,                                              // shufps xmm0,xmm1,imm
        0x0F, 0x11, 0x06, ((DATA + 32) & 0xFF) as u8, ((DATA + 32) >> 8) as u8,       // movups [DATA+32],xmm0
    ];
    run(&mut m, &code);
    let got = (0..16).fold(0u128, |acc, i| {
        acc | ((m.read8(DATA + 32 + i) as u128) << (i * 8))
    });
    assert_eq!(
        got, 0x8888_8888_7777_7777_2222_2222_1111_1111,
        "shufps: 低位は自分・高位は相手から取る"
    );
}
