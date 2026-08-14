//! MMX のテスト。
//!
//! 主役は **libcryptoが踏む列** (movq / pxor / paddq / pmuludq / psrlq) と、
//! **FXSAVE/FXRSTORをまたぐビット同一性** (コンテキストスイッチで壊れないこと)。
//! mmレジスタはx87物理レジスタの仮数64bitへの別名なので、
//! 別名関係とタグの作用 (TOP=0・全valid、EMMSで全empty) も釘で打つ。
//!
//! コードはリアルモード (16bitセグメント) に置く。SSE/MMXの命令選択子は
//! **生の66プレフィクスの有無**で決まり、既定オペランド幅とは無関係 —
//! `0F 6F` は16bitコードでもMMXである (これ自体もこのテストの主張)。

use rustx86_core::{Machine, MachineProfile};

const BASE: u32 = 0x0600;
const DATA: u32 = 0x0700;

/// MMXはFPUの別名なので、FPUを挿した32bit構成で試す
fn mach() -> Machine {
    Machine::with_profile(MachineProfile::pc_32bit(4))
}

/// 命令列を置いて走らせ、HLTまで到達したことを確かめる
fn run(m: &mut Machine, code: &[u8]) {
    for (i, b) in code.iter().enumerate() {
        m.write8(BASE + i as u32, *b);
    }
    m.write8(BASE + code.len() as u32, 0xF4); // hlt
    m.cpu.ip = BASE;
    m.halted = false; // 2周目のrunやスナップショット復元後も走れるように
    m.run(code.len() as u64 + 16);
    assert!(m.trap.is_none(), "trapした: {:?}", m.trap);
    assert!(m.halted, "hltまで到達していない (IPが止まった疑い)");
}

fn write64(m: &mut Machine, a: u32, v: u64) {
    for (i, b) in v.to_le_bytes().iter().enumerate() {
        m.write8(a + i as u32, *b);
    }
}

fn read64(m: &Machine, a: u32) -> u64 {
    (0..8).fold(0u64, |acc, i| acc | (m.read8(a + i) as u64) << (i * 8))
}

/// movq のメモリ往復がビット同一であること。積む値は **f64で表せない
/// ビット列** — f64裏打ちのFPUに素朴に載せると壊れる、libcrypto事故の核心
#[test]
fn movq_round_trips_raw_bits() {
    let mut m = mach();
    write64(&mut m, DATA, 0xDEAD_BEEF_CAFE_BABE);
    run(
        &mut m,
        &[
            0x0F, 0x6F, 0x06, 0x00, 0x07, // movq mm0, [0x700]
            0x0F, 0x7F, 0x06, 0x08, 0x07, // movq [0x708], mm0
        ],
    );
    assert_eq!(read64(&m, DATA + 8), 0xDEAD_BEEF_CAFE_BABE);
}

/// pxor で自身をゼロにし、paddb で飽和なしの折り返しを見る
#[test]
fn pxor_and_paddb_compute() {
    let mut m = mach();
    write64(&mut m, DATA, 0xFF01_02FF_FF80_7F01);
    run(
        &mut m,
        &[
            0x0F, 0xEF, 0xC9, // pxor mm1, mm1
            0x0F, 0x6F, 0x0E, 0x00, 0x07, // movq mm1, [0x700]
            0x0F, 0xFC, 0xC9, // paddb mm1, mm1 (各バイト2倍、折り返し)
            0x0F, 0x7F, 0x0E, 0x08, 0x07, // movq [0x708], mm1
        ],
    );
    // FF+FF=FE, 01+01=02, 02+02=04, 80+80=00, 7F+7F=FE …
    assert_eq!(read64(&m, DATA + 8), 0xFE02_04FE_FE00_FE02);
}

/// pmuludq (下位dword同士のフル積) と psrlq — bn/mont系が踏む形
#[test]
fn pmuludq_and_shift() {
    let mut m = mach();
    write64(&mut m, DATA, 0xFFFF_FFFF); // mm0 = 0xFFFFFFFF (下位dword)
    write64(&mut m, DATA + 8, 0xFFFF_FFFF);
    run(
        &mut m,
        &[
            0x0F, 0x6F, 0x06, 0x00, 0x07, // movq mm0, [0x700]
            0x0F, 0xF4, 0x06, 0x08, 0x07, // pmuludq mm0, [0x708]
            0x0F, 0x73, 0xD0, 0x20, // psrlq mm0, 32
            0x0F, 0x7F, 0x06, 0x10, 0x07, // movq [0x710], mm0
        ],
    );
    // 0xFFFFFFFF² = 0xFFFFFFFE_00000001 → >>32 = 0xFFFFFFFE
    assert_eq!(read64(&m, DATA + 0x10), 0xFFFF_FFFE);
}

/// pshufw と pmaddwd の既知解
#[test]
fn pshufw_and_pmaddwd() {
    let mut m = mach();
    // words: [1, 2, 3, 4]
    write64(&mut m, DATA, 0x0004_0003_0002_0001);
    run(
        &mut m,
        &[
            0x0F, 0x6F, 0x06, 0x00, 0x07, // movq mm0, [0x700]
            0x0F, 0x70, 0xC8, 0x1B, // pshufw mm1, mm0, 0x1B (逆順 [4,3,2,1])
            0x0F, 0x7F, 0x0E, 0x08, 0x07, // movq [0x708], mm1
            0x0F, 0xF5, 0xC8, // pmaddwd mm1, mm0
            0x0F, 0x7F, 0x0E, 0x10, 0x07, // movq [0x710], mm1
        ],
    );
    assert_eq!(read64(&m, DATA + 8), 0x0001_0002_0003_0004, "pshufw逆順");
    // pmaddwd: [4*1+3*2, 2*3+1*4] = [10, 10]
    assert_eq!(read64(&m, DATA + 0x10), 0x0000_000A_0000_000A);
}

/// FXSAVE のST0スロットにMMX値が**指数全1・仮数そのまま**で現れ、
/// FXRSTOR で**ビット同一**に戻ること。カーネルのコンテキストスイッチが
/// この経路なので、ここが崩れるとMMXの計算はスライスをまたぐたびに壊れる
#[test]
fn mmx_survives_fxsave_fxrstor() {
    let mut m = mach();
    let fxarea = 0x0800u32; // 16バイト境界
    write64(&mut m, DATA, 0xDEAD_BEEF_CAFE_BABE);
    run(
        &mut m,
        &[
            0x0F, 0x6F, 0x06, 0x00, 0x07, // movq mm0, [0x700]
            0x0F, 0xAE, 0x06, 0x00, 0x08, // fxsave [0x800]
            0x0F, 0xEF, 0xC0, // pxor mm0, mm0 (壊す)
            0x0F, 0xAE, 0x0E, 0x00, 0x08, // fxrstor [0x800]
            0x0F, 0x7F, 0x06, 0x08, 0x07, // movq [0x708], mm0
        ],
    );
    // FXSAVEのST0域 (+32): 仮数=値、指数=0xFFFF (MMXの実機表現)
    assert_eq!(read64(&m, fxarea + 32), 0xDEAD_BEEF_CAFE_BABE, "仮数");
    assert_eq!(
        m.read16(fxarea + 40),
        0xFFFF,
        "指数全1 (MMXがST枠に見せる顔)"
    );
    assert_eq!(read64(&m, DATA + 8), 0xDEAD_BEEF_CAFE_BABE, "復元後の値");
}

/// MMX命令はTOP=0・タグ全valid、EMMSで全empty (Intel SDMの作法)。
/// x87側から見た世界の入れ替わりを確かめる
#[test]
fn mmx_sets_tags_and_emms_clears_them() {
    let mut m = mach();
    run(
        &mut m,
        &[
            0x0F, 0xEF, 0xC0, // pxor mm0, mm0
        ],
    );
    assert_eq!(m.cpu.fpu.top, 0, "MMX命令の後はTOP=0");
    assert_eq!(m.cpu.fpu.empty, 0, "タグは全valid");

    run(&mut m, &[0x0F, 0x77]); // emms
    assert_eq!(m.cpu.fpu.empty, 0xFF, "EMMSで全empty");
}

/// x87とMMXの**別名関係**: FLDで積んだ値の80bit仮数がmmから見え、
/// MMXで書くとx87のスタックが載っ取られる
#[test]
fn mmx_aliases_the_x87_mantissa() {
    let mut m = mach();
    // 1.5 (f64) をメモリへ → FLD m64。f80仮数は 0xC000000000000000
    write64(&mut m, DATA, 1.5f64.to_bits());
    run(
        &mut m,
        &[
            0xDD, 0x06, 0x00, 0x07, // fld qword [0x700]
            // FLD直後: TOP=7、st(0)=物理R7。mm7 = R7の仮数
            0x0F, 0x7F, 0x3E, 0x08, 0x07, // movq [0x708], mm7
        ],
    );
    assert_eq!(
        read64(&m, DATA + 8),
        0xC000_0000_0000_0000,
        "1.5の80bit仮数"
    );
}

/// スナップショットがMMX値 (80bit原本) を運ぶこと
#[test]
fn snapshot_carries_mmx_state() {
    let mut m = mach();
    write64(&mut m, DATA, 0x0123_4567_89AB_CDEF);
    run(
        &mut m,
        &[
            0x0F, 0x6F, 0x06, 0x00, 0x07, // movq mm0, [0x700]
        ],
    );
    let snap = m.save_state();
    let mut m2 = mach();
    m2.load_state(&snap).expect("復元できる");
    run(
        &mut m2,
        &[
            0x0F, 0x7F, 0x06, 0x08, 0x07, // movq [0x708], mm0
        ],
    );
    assert_eq!(read64(&m2, DATA + 8), 0x0123_4567_89AB_CDEF);
}

/// FLD m80 → FSTP m80 がビット同一で往復すること。f64に落ちない下位11bitを
/// 立てた仮数で確かめる (muslのlong doubleコピーがこの経路)
#[test]
fn f80_load_store_round_trips_exactly() {
    let mut m = mach();
    // 仮数の最下位ビットまで立てた正規化数 (f64では表せない)
    let mant = 0xFFFF_FFFF_FFFF_FFFFu64;
    let se = 0x3FFFu16; // 指数0 (=1.xxx)
    write64(&mut m, DATA, mant);
    m.write16(DATA + 8, se);
    run(
        &mut m,
        &[
            0xDB, 0x2E, 0x00, 0x07, // fld tword [0x700]
            0xDB, 0x3E, 0x10, 0x07, // fstp tword [0x710]
        ],
    );
    assert_eq!(read64(&m, DATA + 0x10), mant, "仮数がビット同一");
    assert_eq!(m.read16(DATA + 0x18), se, "指数もそのまま");
}
