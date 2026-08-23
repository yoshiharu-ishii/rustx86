//! スナップショット v16: Bochs VGA (DISPI) と ATAPI の状態が控えを往復すること。
//! 「CD から起動してログインまで 10 分の機械」を控えて戻すための土台
use rustx86_core::dev::chip::{dispi, ide};
use rustx86_core::{Machine, MachineProfile};

fn dispi_write(m: &mut Machine, idx: u16, v: u16) {
    m.io_write16(dispi::PORT_INDEX, idx);
    m.io_write16(dispi::PORT_DATA, v);
}

fn dispi_read(m: &mut Machine, idx: u16) -> u16 {
    m.io_write16(dispi::PORT_INDEX, idx);
    m.io_read16(dispi::PORT_DATA)
}

#[test]
fn dispi_mode_survives_roundtrip() {
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(64));
    m.vga_attach();
    let base = m.vram_base.expect("VRAM");
    // 1024x768x32 を組む (bochs-drm と同じ手順)
    dispi_write(&mut m, 4, 0);
    dispi_write(&mut m, 1, 1024);
    dispi_write(&mut m, 2, 768);
    dispi_write(&mut m, 3, 32);
    dispi_write(&mut m, 4, dispi::ENABLE_ENABLED | dispi::ENABLE_LFB);
    m.run(10); // sync_dispi は I/O の後に走る
    let lfb = m.lfb.expect("lfb");
    assert_eq!((lfb.width, lfb.height, lfb.base), (1024, 768, base));
    assert!(m.lfb_xrgb);
    // VRAM に画素を置く
    m.write32(base, 0x00FF_0000);

    let data = m.save_state();
    let mut back = Machine::new();
    back.load_state(&data).expect("load");

    assert_eq!(back.vram_base, Some(base));
    assert!(back.lfb_xrgb);
    assert_eq!(back.lfb, Some(lfb));
    assert_eq!(dispi_read(&mut back, 1), 1024);
    assert_eq!(
        dispi_read(&mut back, 4),
        dispi::ENABLE_ENABLED | dispi::ENABLE_LFB
    );
    assert_eq!(back.read32(base), 0x00FF_0000);
    // PCI の顔 (BAR0 = VRAM) も戻る
    let pci = back.devices.pci.as_ref().expect("pci");
    let bar0 = pci.slot(dispi::SLOT).expect("DISPI の関数").bar_base(0);
    assert_eq!(bar0, base, "BAR0 が VRAM を指さない: {bar0:#x}");
    // 戻した機械でも DISPI の書き込みが lfb に効く
    dispi_write(&mut back, 2, 600);
    back.run(10);
    assert_eq!(back.lfb.unwrap().height, 600);
}

#[test]
fn atapi_state_survives_roundtrip_and_image_is_reinserted() {
    let iso = vec![0xA5u8; 2048 * 4];
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(32));
    m.cd_attach(iso.clone());
    // IDENTIFY PACKET DEVICE (0xA1) を叩いて DRQ の途中で控える
    m.io_write8(ide::BASE + 6, 0xB0); // slave… secondary の master を選ぶ (drive 0)
    m.io_write8(ide::BASE + 6, 0xA0);
    m.io_write8(ide::BASE + 7, 0xA1);
    m.run(10);
    let status = m.io_read8(ide::BASE + 7);
    assert!(status & 0x08 != 0, "DRQ が立たない: {status:#x}");
    let w0 = m.io_read16(ide::BASE);
    assert_eq!(w0, 0x8580, "IDENTIFY の word0");

    let data = m.save_state();
    assert!(
        data.len() < iso.len() + 2048 * 2 + (32 << 20),
        "像が控えに混ざっている"
    );
    let mut back = Machine::new();
    back.load_state(&data).expect("load");
    assert!(back.cd_wanted(), "素子はあるが像が無い、の印");
    // 残りの IDENTIFY データは素子の中 — 像なしでも続きが読める
    let w1 = back.io_read16(ide::BASE);
    assert_eq!(w1, m.io_read16(ide::BASE), "DRQ の続きが元の機械と同じ");
    back.cd_attach(iso);
    assert!(!back.cd_wanted());
    // 像の中身が読める (READ(10) で LBA 0 を 1 セクタ)
    back.run(10_000);
    for _ in 0..256 {
        back.io_read16(ide::BASE); // IDENTIFY の残りを吸い切る
    }
    back.io_write8(ide::BASE + 4, 0x00);
    back.io_write8(ide::BASE + 5, 0x08); // 上限 2048
    back.io_write8(ide::BASE + 7, 0xA0); // PACKET
    back.run(10);
    let pkt = [0x28u8, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0];
    for w in pkt.chunks_exact(2) {
        back.io_write16(ide::BASE, u16::from_le_bytes([w[0], w[1]]));
    }
    back.run(10);
    let status = back.io_read8(ide::BASE + 7);
    assert!(
        status & 0x08 != 0,
        "READ(10) の DRQ が立たない: {status:#x}"
    );
    let first = back.io_read16(ide::BASE);
    assert_eq!(first, 0xA5A5, "挿し直した像のセクタが読める");
}
