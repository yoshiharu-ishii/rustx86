//! bzImage セットアップヘッダの解釈 (Tier 3b-1)。
//!
//! 実カーネルが無くても、**ヘッダの読み取りは決定的にテストできる**。
//! 合成のヘッダを作って、正しく読めること・不正を正しく断ることを固める。

use rustx86_core::bzimage::SetupHeader;

/// 最小の有効な bzImage ヘッダを持つバイト列を作る
fn fake_bzimage(setup_sects: u8, version: u16, code32: u32, loadflags: u8) -> Vec<u8> {
    let mut b = vec![0u8; 0x1000];
    b[0x1F1] = setup_sects;
    b[0x202..0x206].copy_from_slice(b"HdrS");
    b[0x206..0x208].copy_from_slice(&version.to_le_bytes());
    b[0x211] = loadflags;
    b[0x214..0x218].copy_from_slice(&code32.to_le_bytes());
    b
}

#[test]
fn 正しいヘッダを読める() {
    let img = fake_bzimage(4, 0x020C, 0x0010_0000, 0x01);
    let h = SetupHeader::parse(&img).expect("読めるべき");
    assert_eq!(h.setup_sects, 4);
    assert_eq!(h.version, 0x020C);
    assert_eq!(h.code32_start, 0x0010_0000);
    assert!(h.loaded_high());
    // カーネル本体は 先頭512 + setup 4セクタ = 0xA00 から
    assert_eq!(h.kernel_offset(), (4 + 1) * 512);
}

#[test]
fn setup_sects_0は4と読む() {
    // レガシーの罠: 0 は「実は4」
    let img = fake_bzimage(0, 0x0206, 0x0010_0000, 0x01);
    let h = SetupHeader::parse(&img).unwrap();
    assert_eq!(h.setup_sects, 4);
    assert_eq!(h.kernel_offset(), (4 + 1) * 512);
}

#[test]
fn マジックが無ければ断る() {
    let mut img = fake_bzimage(4, 0x0206, 0x0010_0000, 0x01);
    img[0x202] = b'X'; // マジックを壊す
    let e = SetupHeader::parse(&img).unwrap_err();
    assert!(e.contains("HdrS"), "理由がマジック不在でない: {e}");
}

#[test]
fn 古い版は断る() {
    let img = fake_bzimage(4, 0x0106, 0x0010_0000, 0x01); // 1.06
    let e = SetupHeader::parse(&img).unwrap_err();
    assert!(e.contains("古すぎる"), "理由が版古すぎでない: {e}");
}

#[test]
fn 短すぎるイメージは断る() {
    let e = SetupHeader::parse(&[0u8; 100]).unwrap_err();
    assert!(e.contains("短すぎる"), "理由が短すぎでない: {e}");
}

// ---- zero page (3b-2) ----

use rustx86_core::bzimage::{build_zero_page, zero_page_e820, zero_page_e820_count};

#[test]
fn e820はramサイズから作られる() {
    let img = fake_bzimage(4, 0x020C, 0x0010_0000, 0x01);
    // 128MB の機械
    let zp = build_zero_page(&img, 128 << 20, 0x9_0000, None);
    // 640K + EBDA + VGA窓 + 1MB以降 = 4エントリ
    assert_eq!(zero_page_e820_count(&zp), 4);
    // 最後のエントリ = 1MB から (128MB - 1MB) の使えるRAM
    let (base, size, kind) = zero_page_e820(&zp, 3);
    assert_eq!(base, 0x0010_0000);
    assert_eq!(size, (128u64 << 20) - 0x0010_0000);
    assert_eq!(kind, 1, "1MB以降が使えるRAM(kind=1)でない");
}

#[test]
fn 小さいramでは1mb以降のエントリが無い() {
    let img = fake_bzimage(4, 0x020C, 0x0010_0000, 0x01);
    // 1MB ちょうど → 1MB以降のRAMが無いので3エントリ
    let zp = build_zero_page(&img, 1 << 20, 0x9_0000, None);
    assert_eq!(zero_page_e820_count(&zp), 3);
}

#[test]
fn zero_pageにヘッダとcmdlineが入る() {
    let img = fake_bzimage(4, 0x020C, 0x0010_0000, 0x01);
    let zp = build_zero_page(&img, 128 << 20, 0x9_0000, None);
    // setupヘッダのマジックが zero page にも写っている
    assert_eq!(&zp[0x202..0x206], b"HdrS");
    // cmdline ポインタ
    assert_eq!(
        u32::from_le_bytes(zp[0x228..0x22C].try_into().unwrap()),
        0x9_0000
    );
}

// ---- boot_bzimage: ロードして32bitで走る (3b-3) ----

use rustx86_core::{cpu, Machine, MachineProfile};

/// 32bitカーネル本体が「EAXに目印を書いてHLT」する最小の合成bzImage。
/// setup 1セクタ + カーネル本体、という最小の形に包む
fn fake_bzimage_with_kernel(kernel: &[u8], code32_start: u32) -> Vec<u8> {
    let setup_sects = 1u8;
    let mut img = fake_bzimage(setup_sects, 0x020C, code32_start, 0x01);
    // カーネル本体を kernel_offset = (1+1)*512 = 0x400 に置く
    let off = (setup_sects as usize + 1) * 512;
    if img.len() < off + kernel.len() {
        img.resize(off + kernel.len(), 0);
    }
    img[off..off + kernel.len()].copy_from_slice(kernel);
    img
}

#[test]
fn bzimageをロードして32bitカーネルが走る() {
    // mov eax, 0x1BADB002 ; hlt  (物理1MBに置かれ、そこから走る)
    let kernel = [0xB8, 0x02, 0xB0, 0xAD, 0x1B, 0xF4];
    let img = fake_bzimage_with_kernel(&kernel, 0x0010_0000);

    let mut m = Machine::with_profile(MachineProfile::pc_32bit(128));
    m.boot_bzimage(&img, "console=ttyS0").expect("boot");

    // 入った瞬間: 32bit protected mode、%esi = zero page、CS.base=0
    assert!(m.cpu.pe(), "protected modeに入っていない");
    assert!(m.cpu.seg_is32(cpu::CS), "32bitセグメントでない");
    assert_eq!(
        m.cpu.regs[cpu::SI],
        0x0001_0000,
        "%esi が zero page を指していない"
    );
    assert_eq!(m.cpu.ip, 0x0010_0000, "エントリが code32_start でない");

    // 走らせる → カーネルが目印を書いて HLT
    for _ in 0..100 {
        if m.halted {
            break;
        }
        m.step();
    }
    assert!(m.halted, "HLTに到達していない");
    assert_eq!(m.cpu.regs[cpu::AX], 0x1BAD_B002, "カーネルが走っていない");
}

#[test]
fn カーネルは物理1mbへ載る() {
    let kernel = [0xB8, 0x02, 0xB0, 0xAD, 0x1B, 0xF4];
    let img = fake_bzimage_with_kernel(&kernel, 0x0010_0000);
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(128));
    m.boot_bzimage(&img, "").unwrap();
    // 物理1MBにカーネルの先頭バイトが載っている
    assert_eq!(m.read_phys8(0x0010_0000), 0xB8);
}

#[test]
fn zero_pageが物理0x10000に載る() {
    let kernel = [0xF4];
    let img = fake_bzimage_with_kernel(&kernel, 0x0010_0000);
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(64));
    m.boot_bzimage(&img, "").unwrap();
    // zero page のマジックが物理0x10000+0x202に載っている
    assert_eq!(&[m.read_phys8(0x1_0202), m.read_phys8(0x1_0203)], b"Hd");
}
